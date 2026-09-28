//! The Media Foundation capture path (ADR-0038, preferred by ADR-0039).
//!
//! Pull-based from a dedicated OS thread, like `capture.rs`. Unlike it, the
//! clock belongs to the device: `ReadSample` blocks until the camera has a
//! frame, so the frame rate is negotiated once and then obeyed, instead of being
//! polled for.
//!
//! **The format is ours to choose, and the conversion ours to do.** The device
//! is put in one of its own formats — uncompressed when it has one at the size
//! and rate we want — and `convert.rs` turns that into NV12, the same code the
//! DirectShow path uses. Windows only decodes: when the best format is MJPEG or
//! H.264, the reader inserts a decoder and we ask it for a layout we read.
//!
//! This module used to ask the reader for NV12 and trust
//! `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING` to produce it. That attribute
//! converts YUV to RGB-32 and nothing else, so every camera that did not emit
//! NV12 natively failed with `MF_E_TOPO_CODEC_NOT_FOUND` — and failed after
//! the track was already published (ADR-0039, amendment of 2026-09-27).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;

use livekit::webrtc::video_frame::NV12Buffer;
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample,
    IMFSourceReader, MFCreateAttributes, MFCreateDeviceSource, MFCreateMediaType,
    MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFNominalRange_16_235, MFShutdown, MFStartup, MFVideoFormat_I420, MFVideoFormat_NV12,
    MFVideoFormat_RGB32, MFVideoFormat_YUY2, MFSTARTUP_FULL, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READERF_ERROR, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use super::convert::{Converter, Pixels, Raw, PREFERENCE};
use super::{choose_format, CameraDevice, CameraError, Format, Frames, OnLost, Refused};
use crate::capture::Size;

/// `HRESULT` of "another application already has the camera".
const ERROR_SHARING_VIOLATION: i32 = -2147024864; // 0x80070020
/// `HRESULT` of "the camera privacy setting says no".
const E_ACCESSDENIED: i32 = -2147024891; // 0x80070005
/// `MF_E_HW_MFT_FAILED_START_STREAMING`: the driver refused to start, which in
/// practice means the same thing as the sharing violation above.
const MF_E_HW_MFT_FAILED_START_STREAMING: i32 = -1072873339; // 0xC00D3E85
/// `MF_E_NO_MORE_TYPES`, the end of the format list. Not an error.
const MF_E_NO_MORE_TYPES: i32 = -1072875847; // 0xC00D36B9
/// The symbolic link no longer names a device: unplugged between listing and
/// choosing, or a virtual camera whose source went away.
const ERROR_FILE_NOT_FOUND: i32 = -2147024894; // 0x80070002
const MF_E_NOT_FOUND: i32 = -1072875819; // 0xC00D36D5
const E_INVALIDARG: i32 = -2147024809; // 0x80070057

impl CameraError {
    pub(super) fn from_hresult(error: &windows::core::Error, context: &str) -> Self {
        match error.code().0 {
            ERROR_SHARING_VIOLATION | MF_E_HW_MFT_FAILED_START_STREAMING => Self::Busy,
            E_ACCESSDENIED => Self::NotAllowed,
            _ => Self::Platform(format!("{context}: {error}")),
        }
    }
}

/// Media Foundation, started for as long as this value lives.
///
/// `MFStartup` is reference counted per process, but COM apartment state is per
/// **thread**, so both are taken here and given back together: the enumeration
/// call and the capture thread each hold one, and neither has to know about the
/// other.
struct Session;

impl Session {
    fn new() -> Result<Self, CameraError> {
        unsafe {
            // Já inicializado por outra parte do processo é um caso normal, e o
            // `HRESULT` de aviso não é falha: só não devolvemos a inicialização
            // que não fizemos, o que o `CoUninitialize` pareado resolve.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)
                .map_err(|e| CameraError::from_hresult(&e, "iniciando o Media Foundation"))?;
        }
        Ok(Self)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

/// Attributes that say "video capture devices".
fn vidcap_attributes(extra: u32) -> Result<IMFAttributes, CameraError> {
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 1 + extra)
            .map_err(|e| CameraError::from_hresult(&e, "criando atributos"))?;
        let attributes = attributes.ok_or(CameraError::NoUsableFormat)?;
        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| CameraError::from_hresult(&e, "pedindo dispositivos de video"))?;
        Ok(attributes)
    }
}

/// Reads one `CoTaskMem` string attribute and frees it.
fn allocated_string(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    unsafe {
        let mut value = PWSTR::null();
        let mut length = 0u32;
        attributes
            .GetAllocatedString(key, &mut value, &mut length)
            .ok()?;
        let text = value.to_string().ok();
        CoTaskMemFree(Some(value.as_ptr().cast()));
        text
    }
}

/// Lists the cameras Media Foundation knows, in the order Windows reports them.
pub(super) fn list() -> Vec<CameraDevice> {
    let Ok(_session) = Session::new() else {
        return Vec::new();
    };
    let Ok(attributes) = vidcap_attributes(0) else {
        return Vec::new();
    };

    let mut devices = Vec::new();
    unsafe {
        let mut raw: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFEnumDeviceSources(&attributes, &mut raw, &mut count).is_err() {
            return devices;
        }

        for index in 0..count as usize {
            // Tira o ponteiro do array: a partir daqui quem libera é o `Drop` do
            // `IMFActivate`, e não o `CoTaskMemFree` do array.
            let Some(activate) = std::ptr::read(raw.add(index)) else {
                continue;
            };
            let attributes: IMFAttributes = activate.cast().unwrap_or_else(|_| activate.into());
            let id = allocated_string(
                &attributes,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            );
            let name = allocated_string(&attributes, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME);
            if let Some(id) = id {
                devices.push(CameraDevice {
                    name: name.unwrap_or_else(|| "Câmera".to_owned()),
                    id,
                });
            }
        }
        CoTaskMemFree(Some(raw.cast()));
    }
    devices
}

/// The friendly name Media Foundation gives `device_id`, if it still has it.
pub(super) fn name_of(device_id: &str) -> Option<String> {
    list()
        .into_iter()
        .find(|d| d.id == device_id)
        .map(|d| d.name)
}

/// Opens one camera by symbolic link, without enumerating again.
fn open_device(device_id: &str) -> Result<IMFMediaSource, CameraError> {
    let attributes = vidcap_attributes(1)?;
    unsafe {
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        attributes
            .SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                PWSTR(wide.as_ptr() as *mut u16),
            )
            .map_err(|e| CameraError::from_hresult(&e, "escolhendo a camera"))?;

        MFCreateDeviceSource(&attributes).map_err(|e| {
            let code = e.code().0;
            eprintln!("camera: MFCreateDeviceSource recusou com {code:#010x}");
            match code {
                // Sumiu entre listar e escolher — ou é uma câmera virtual cuja
                // inscrição no MF ficou para trás. Os dois casos merecem a
                // tentativa pelo DirectShow (ADR-0039).
                ERROR_FILE_NOT_FOUND | MF_E_NOT_FOUND => CameraError::Gone,
                // O dispositivo **está** ali — a enumeração acabou de devolvê-lo
                // — e a ativação recusa mesmo assim. É o que uma câmera virtual
                // só-DirectShow faz: aparece no MF e não abre por ele.
                E_INVALIDARG => CameraError::WillNotOpen(code as u32),
                _ => CameraError::from_hresult(&e, "abrindo a camera"),
            }
        })
    }
}

/// Opens and immediately closes `device_id`, to learn whether this path opens it.
///
/// Only the diagnostic test uses it. Starting a capture opens the device once,
/// on its own thread, and negotiates there: a separate probe meant two opens per
/// start, and it answered the wrong question — a camera can open here and still
/// offer nothing the reader will convert.
#[cfg(test)]
pub(super) fn probe(device_id: &str) -> Result<(), CameraError> {
    let _session = Session::new()?;
    let source = open_device(device_id)?;
    unsafe {
        let _ = source.Shutdown();
    }
    Ok(())
}

fn frame_size(media_type: &IMFMediaType) -> Option<Size> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    Some(Size {
        width: (packed >> 32) as u32,
        height: (packed & 0xFFFF_FFFF) as u32,
    })
}

fn frame_rate(media_type: &IMFMediaType) -> Option<u32> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_RATE) }.ok()?;
    let numerator = (packed >> 32) as u32;
    let denominator = (packed & 0xFFFF_FFFF) as u32;
    (denominator > 0).then(|| numerator / denominator)
}

/// One format the device offers, as the reader lists it.
#[derive(Debug, Clone, Copy)]
struct Native {
    index: u32,
    format: Format,
    /// `None` for compressed video — MJPEG, H.264 — which needs a decoder.
    pixels: Option<Pixels>,
}

/// How many of the device's formats to try before giving up on this path.
///
/// Each attempt reconfigures the device, which some drivers take tens of
/// milliseconds to do. Past this, DirectShow is the better bet.
const MAX_ATTEMPTS: usize = 6;

/// The order to try the device's formats in.
///
/// The best size and frame rate first, by the same rule as DirectShow; among
/// formats that tie, the uncompressed ones first, cheapest to convert first
/// (`PREFERENCE`), and compressed last. Then, if the best size only exists
/// compressed, the best **uncompressed** format behind it: a decoder can be
/// missing on a stripped-down Windows, and a smaller picture beats none.
fn ranked(natives: &[Native], ceiling: Size, fps: u32) -> Vec<Native> {
    let cost = |native: &Native| match native.pixels {
        Some(pixels) => PREFERENCE
            .iter()
            .position(|preferred| *preferred == pixels)
            .unwrap_or(PREFERENCE.len()),
        None => PREFERENCE.len() + 1,
    };
    let best_of = |pool: &[Native]| -> Vec<Native> {
        let formats: Vec<Format> = pool.iter().map(|native| native.format).collect();
        let Some(best) = choose_format(&formats, ceiling, fps) else {
            return Vec::new();
        };
        let mut tied: Vec<Native> = pool
            .iter()
            .filter(|native| native.format.size == best.size && native.format.fps == best.fps)
            .copied()
            .collect();
        tied.sort_by_key(cost);
        tied
    };

    let mut order = best_of(natives);
    let uncompressed: Vec<Native> = natives
        .iter()
        .filter(|native| native.pixels.is_some())
        .copied()
        .collect();
    for native in best_of(&uncompressed) {
        if !order.iter().any(|tried| tried.index == native.index) {
            order.push(native);
        }
    }
    order.truncate(MAX_ATTEMPTS);
    order
}

/// Every format the device offers on its first video stream.
fn native_formats(reader: &IMFSourceReader) -> Result<Vec<Native>, CameraError> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut natives = Vec::new();
    for index in 0u32.. {
        let offered = match unsafe { reader.GetNativeMediaType(stream, index) } {
            Ok(offered) => offered,
            Err(error) if error.code().0 == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(CameraError::from_hresult(&error, "lendo os formatos")),
        };
        let Some(size) = frame_size(&offered) else {
            continue;
        };
        if size.width < 2 || size.height < 2 {
            continue;
        }
        let pixels = unsafe { offered.GetGUID(&MF_MT_SUBTYPE) }
            .ok()
            .and_then(|subtype| Pixels::from_subtype(&subtype));
        natives.push(Native {
            index,
            format: Format {
                size,
                fps: frame_rate(&offered).unwrap_or(30),
            },
            pixels,
        });
    }
    Ok(natives)
}

/// What the reader was talked into delivering, and how to read it.
#[derive(Debug, Clone, Copy)]
struct Negotiated {
    size: Size,
    pixels: Pixels,
    /// `MF_MT_DEFAULT_STRIDE` of the output, when the type states one. A
    /// negative value is a bottom-up picture. Only a buffer without
    /// `IMF2DBuffer` needs it; the 2-D ones say their own pitch.
    stride: Option<i32>,
}

impl Negotiated {
    /// Reads the output type the reader settled on.
    ///
    /// What counts is what the reader says it will deliver, not what was asked
    /// for: it can accept a request and adjust it.
    fn current(reader: &IMFSourceReader) -> Result<Self, CameraError> {
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let current = unsafe { reader.GetCurrentMediaType(stream) }
            .map_err(|e| CameraError::from_hresult(&e, "lendo o formato negociado"))?;
        let subtype = unsafe { current.GetGUID(&MF_MT_SUBTYPE) }
            .map_err(|e| CameraError::from_hresult(&e, "lendo o formato negociado"))?;
        let pixels = Pixels::from_subtype(&subtype).ok_or(CameraError::NoUsableFormat)?;
        let size = frame_size(&current).ok_or(CameraError::NoUsableFormat)?;
        let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .ok()
            .map(|stride| stride as i32);
        Ok(Self {
            size,
            pixels,
            stride,
        })
    }
}

/// Uncompressed outputs to ask a decoder for, in the order we would rather
/// have them. All four are layouts `convert.rs` reads.
const DECODED: [GUID; 4] = [
    MFVideoFormat_NV12,
    MFVideoFormat_YUY2,
    MFVideoFormat_I420,
    MFVideoFormat_RGB32,
];

/// Puts the device in one native format and gets frames we can read out of it.
fn try_native(reader: &IMFSourceReader, native: Native) -> Result<Negotiated, CameraError> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let offered = unsafe { reader.GetNativeMediaType(stream, native.index) }
        .map_err(|e| CameraError::from_hresult(&e, "relendo o formato escolhido"))?;

    // Fixar o tipo nativo é o que escolhe o formato **do dispositivo**. Pedir
    // só a saída deixa o leitor escolher a entrada sozinho, e ele não escolhe
    // pela nossa lista.
    unsafe { reader.SetCurrentMediaType(stream, None, &offered) }
        .map_err(|e| CameraError::from_hresult(&e, "fixando o formato da camera"))?;

    if native.pixels.is_none() {
        // Comprimido: um decodificador precisa entrar. Pedimos, em ordem, as
        // saídas que sabemos converter, com o tamanho e a taxa do formato
        // escolhido. O processamento avançado do leitor cobre o que o
        // decodificador sozinho não entrega.
        //
        // E a faixa **limitada**, dita por extenso. JPEG é YUV de faixa cheia
        // (0–255), o decodificador de MJPEG entrega assim, e o conversor do
        // Windows mantém a faixa se ninguém pedir outra. O encoder lê 16–235:
        // sem isto, toda webcam MJPEG sairia com o branco estourado e o preto
        // esmagado. O teste com arquivo MJPEG mediu Y=255 onde devia ser 235.
        let mut refusal = None;
        let decoded = DECODED.iter().any(|subtype| {
            let attempt = unsafe {
                MFCreateMediaType().and_then(|wanted| {
                    wanted.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                    wanted.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                    wanted.SetUINT64(&MF_MT_FRAME_SIZE, offered.GetUINT64(&MF_MT_FRAME_SIZE)?)?;
                    if let Ok(rate) = offered.GetUINT64(&MF_MT_FRAME_RATE) {
                        wanted.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
                    }
                    wanted.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
                    reader.SetCurrentMediaType(stream, None, &wanted)
                })
            };
            match attempt {
                Ok(()) => true,
                Err(error) => {
                    refusal = Some(error);
                    false
                }
            }
        });
        if !decoded {
            return Err(match refusal {
                Some(error) => CameraError::from_hresult(&error, "decodificando o video da camera"),
                None => CameraError::OnlyCompressed,
            });
        }
    }

    Negotiated::current(reader)
}

/// Settles the reader on a format we can read, trying the device's own formats
/// in `ranked` order.
fn negotiate(reader: &IMFSourceReader, ceiling: Size, fps: u32) -> Result<Negotiated, CameraError> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    // Só a trilha de vídeo: câmeras com foto, profundidade ou infravermelho
    // expõem outras, e uma trilha selecionada que ninguém lê segura o leitor.
    unsafe {
        let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
        reader
            .SetStreamSelection(stream, true)
            .map_err(|e| CameraError::from_hresult(&e, "ligando a trilha de video"))?;
    }

    let natives = native_formats(reader)?;
    let mut last = None;
    for native in ranked(&natives, ceiling, fps) {
        match try_native(reader, native) {
            Ok(negotiated) => return Ok(negotiated),
            Err(error @ (CameraError::Busy | CameraError::NotAllowed)) => return Err(error),
            Err(error) => {
                eprintln!(
                    "camera: o Media Foundation recusou {} {}x{}: {error}",
                    native
                        .pixels
                        .map(|pixels| format!("{pixels:?}"))
                        .unwrap_or_else(|| "comprimido".to_owned()),
                    native.format.size.width,
                    native.format.size.height,
                );
                last = Some(error);
            }
        }
    }
    Err(last.unwrap_or(CameraError::NoUsableFormat))
}

/// The attributes every reader of ours is created with.
///
/// **Advanced** video processing, and not the basic kind. The basic one,
/// `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`, converts only YUV to RGB-32 — it
/// never produced NV12 from anything, which is exactly the failure users hit
/// (`MF_E_TOPO_CODEC_NOT_FOUND` on "pedindo NV12"). The advanced one inserts a
/// real video processor. It is now a second line of defence: `negotiate` asks
/// for formats that need no conversion first.
fn reader_attributes() -> Result<IMFAttributes, CameraError> {
    unsafe {
        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 1)
            .map_err(|e| CameraError::from_hresult(&e, "criando atributos do leitor"))?;
        let attributes = attributes.ok_or(CameraError::NoUsableFormat)?;
        attributes
            .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
            .map_err(|e| CameraError::from_hresult(&e, "ligando a conversao de formato"))?;
        Ok(attributes)
    }
}

/// An open device, its reader, and the session all three depend on.
///
/// One value so the teardown cannot be forgotten and cannot run in the wrong
/// order: `Drop` shuts the source down, then the fields go in declaration
/// order, which puts the Media Foundation session last.
struct Opened {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    negotiated: Negotiated,
    _session: Session,
}

impl Opened {
    fn new(device_id: &str, ceiling: Size, fps: u32) -> Result<Self, CameraError> {
        let session = Session::new()?;
        let source = open_device(device_id)?;
        let prepared = reader_attributes().and_then(|attributes| {
            let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &attributes) }
                .map_err(|e| CameraError::from_hresult(&e, "abrindo o leitor"))?;
            let negotiated = negotiate(&reader, ceiling, fps)?;
            Ok((reader, negotiated))
        });
        match prepared {
            Ok((reader, negotiated)) => Ok(Self {
                reader,
                source,
                negotiated,
                _session: session,
            }),
            Err(error) => {
                // Aberta e não usada ainda precisa ser desligada, ou a câmera
                // fica ocupada para o DirectShow que vem logo depois.
                unsafe {
                    let _ = source.Shutdown();
                }
                Err(error)
            }
        }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            let _ = self.source.Shutdown();
        }
    }
}

/// A running Media Foundation capture.
pub(super) struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    pub(super) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Opens the camera, negotiates, and starts reading — all on its own thread.
///
/// The outcome comes back over a channel **before** this returns. That is the
/// whole fix for the failure users hit: opening used to be proven here and the
/// format negotiated later, inside the thread, after the track was already on
/// the air — so a camera that opened but offered nothing convertible ended up
/// as a black tile and "a câmera foi encerrada", and DirectShow was never
/// tried. Now a refusal of any kind is an answer, and the frame sink comes home
/// with it so DirectShow can be tried next (ADR-0039).
pub(super) fn start(
    device_id: &str,
    ceiling: Size,
    fps: u32,
    frames: Frames,
    on_lost: OnLost,
) -> Result<Capture, Refused> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let device = device_id.to_owned();
    let (ready, opened) = mpsc::channel::<Result<(), (CameraError, Frames)>>();

    let thread = std::thread::Builder::new()
        .name("ldktela-camera".into())
        .spawn(move || {
            let mut frames = frames;
            let camera = match Opened::new(&device, ceiling, fps) {
                Ok(camera) => camera,
                Err(error) => {
                    let _ = ready.send(Err((error, frames)));
                    return;
                }
            };
            let chosen = camera.negotiated;
            eprintln!(
                "camera: Media Foundation negociou {:?} {}x{}",
                chosen.pixels, chosen.size.width, chosen.size.height
            );
            let _ = ready.send(Ok(()));

            if let Err(error) = run(&camera, &mut frames, &thread_stop) {
                eprintln!("camera: captura interrompida: {error}");
                on_lost(error.to_string());
            }
        })
        .map_err(|_| Refused {
            error: CameraError::Platform("nao consegui criar a thread".into()),
            frames: None,
        })?;

    match opened.recv() {
        Ok(Ok(())) => Ok(Capture {
            stop,
            thread: Some(thread),
        }),
        Ok(Err((error, frames))) => {
            let _ = thread.join();
            Err(Refused {
                error,
                frames: Some(Box::new(frames)),
            })
        }
        Err(_) => Err(Refused {
            error: CameraError::Platform("a thread da camera terminou antes de abrir".into()),
            frames: None,
        }),
    }
}

fn run(camera: &Opened, frames: &mut Frames, stop: &AtomicBool) -> Result<(), CameraError> {
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    let mut layout = camera.negotiated;
    let mut converter = Converter::new(layout.pixels);

    while !stop.load(Ordering::Relaxed) {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        unsafe {
            camera
                .reader
                .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                .map_err(|e| CameraError::from_hresult(&e, "lendo um quadro"))?;
        }

        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(CameraError::Gone);
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            // A câmera foi desconectada. Não é o mesmo que parar: quem
            // compartilha precisa saber.
            return Err(CameraError::Gone);
        }
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
            // O driver trocou o formato no meio do caminho — acontece ao mudar
            // a resolução numa câmera virtual. Seguir lendo com o layout velho
            // leria cada quadro com o tamanho errado.
            layout = Negotiated::current(&camera.reader)?;
            if converter.kind() != layout.pixels {
                converter = Converter::new(layout.pixels);
            }
        }
        // Sem amostra é rotina: o leitor devolve vazio quando o dispositivo
        // ainda está acordando.
        let Some(sample) = sample else {
            continue;
        };

        frames.deliver(layout.size, |nv12| {
            read_frame(&sample, &layout, &mut converter, nv12)
        });
    }
    Ok(())
}

/// Hands one sample's pixels to the converter, whichever way the buffer is laid out.
///
/// `IMF2DBuffer` is the right door when it exists: `Lock2D` points at the top
/// row and says the pitch, negative for a bottom-up picture. A plain buffer
/// starts at the first row in memory, and only the media type's default stride
/// says which way up that is.
fn read_frame(
    sample: &IMFSample,
    layout: &Negotiated,
    converter: &mut Converter,
    dst: &mut NV12Buffer,
) -> bool {
    let size = layout.size;
    let rows = size.height as usize;
    unsafe {
        let Ok(buffer) = sample.ConvertToContiguousBuffer() else {
            return false;
        };

        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let mut scanline = std::ptr::null_mut();
            let mut pitch = 0i32;
            if two_d.Lock2D(&mut scanline, &mut pitch).is_err() {
                return false;
            }
            let span = pitch.unsigned_abs() as usize;
            let ok = !scanline.is_null() && span > 0 && {
                let raw = if pitch > 0 {
                    Raw {
                        data: std::slice::from_raw_parts(
                            scanline,
                            layout.pixels.needed(span, size.height),
                        ),
                        pitch: span,
                        bottom_up: false,
                    }
                } else {
                    // A linha de cima está no fim do bloco; o bloco começa na
                    // de baixo, `rows - 1` passos antes.
                    let first = scanline.sub(span * (rows - 1));
                    Raw {
                        data: std::slice::from_raw_parts(first, span * rows),
                        pitch: span,
                        bottom_up: true,
                    }
                };
                converter.write(raw, size, dst)
            };
            let _ = two_d.Unlock2D();
            return ok;
        }

        let mut data = std::ptr::null_mut();
        let mut length = 0u32;
        if buffer.Lock(&mut data, None, Some(&mut length)).is_err() {
            return false;
        }
        let stride = layout
            .stride
            .unwrap_or(layout.pixels.stride(size.width) as i32);
        let ok = !data.is_null() && stride != 0 && {
            let raw = Raw {
                data: std::slice::from_raw_parts(data, length as usize),
                pitch: stride.unsigned_abs() as usize,
                bottom_up: stride < 0,
            };
            converter.write(raw, size, dst)
        };
        let _ = buffer.Unlock();
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::fixture::{expected_luma, write_avi, Layout};
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::MFCreateSourceReaderFromURL;

    const HD: Size = Size {
        width: 1280,
        height: 720,
    };

    fn native(index: u32, width: u32, height: u32, fps: u32, pixels: Option<Pixels>) -> Native {
        Native {
            index,
            format: Format {
                size: Size { width, height },
                fps,
            },
            pixels,
        }
    }

    fn indices(order: &[Native]) -> Vec<u32> {
        order.iter().map(|native| native.index).collect()
    }

    /// Mesmo tamanho e mesma taxa: o que não precisa de decodificador vai na
    /// frente, e entre esses o mais barato de converter.
    #[test]
    fn an_uncompressed_format_is_tried_before_a_compressed_twin() {
        let natives = [
            native(0, 1280, 720, 30, None),
            native(1, 1280, 720, 30, Some(Pixels::Rgb24)),
            native(2, 1280, 720, 30, Some(Pixels::Yuy2)),
        ];
        assert_eq!(indices(&ranked(&natives, HD, 30)), [2, 1, 0]);
    }

    /// O caso de quase toda webcam USB: 720p30 só em MJPEG, e em YUY2 só a 10
    /// quadros. O rosto a 30 quadros vence; o YUY2 fica de reserva.
    #[test]
    fn a_compressed_format_wins_when_it_is_the_only_one_at_full_rate() {
        let natives = [
            native(0, 1280, 720, 10, Some(Pixels::Yuy2)),
            native(1, 1280, 720, 30, None),
            native(2, 640, 480, 30, Some(Pixels::Yuy2)),
        ];
        let order = indices(&ranked(&natives, HD, 30));
        assert_eq!(order[0], 1, "MJPEG 720p30 primeiro");
        assert!(
            order.len() > 1 && order[1..].contains(&0),
            "um formato sem compressao fica de reserva, caso o decodificador falte: {order:?}"
        );
    }

    #[test]
    fn a_device_with_nothing_to_offer_yields_nothing_to_try() {
        assert!(ranked(&[], HD, 30).is_empty());
    }

    #[test]
    fn the_attempts_are_bounded() {
        let natives: Vec<Native> = (0..20)
            .map(|index| native(index, 640, 480, 30, None))
            .collect();
        assert!(ranked(&natives, HD, 30).len() <= MAX_ATTEMPTS);
    }

    /// Um leitor sobre um arquivo, com os mesmos atributos de um sobre a
    /// câmera. `None` quando esta máquina não tem o Media Foundation ou o
    /// leitor de AVI dele — o Windows Server do CI pode não ter —, e só então.
    fn reader_over(path: &std::path::Path) -> Option<IMFSourceReader> {
        let attributes = reader_attributes().ok()?;
        match unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes) }
        {
            Ok(reader) => Some(reader),
            Err(error) => {
                eprintln!("sem leitor de AVI nesta maquina ({error}); nada a provar aqui");
                None
            }
        }
    }

    fn first_sample(reader: &IMFSourceReader) -> IMFSample {
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        for _ in 0..10 {
            let mut flags = 0u32;
            let mut sample = None;
            unsafe {
                reader
                    .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                    .expect("lendo uma amostra");
            }
            if let Some(sample) = sample {
                return sample;
            }
        }
        panic!("o leitor nao entregou amostra nenhuma");
    }

    /// O defeito relatado, reproduzido e resolvido: uma fonte que o Media
    /// Foundation abre mas que não oferece NV12. O código antigo pedia NV12 e
    /// recebia `MF_E_TOPO_CODEC_NOT_FOUND` (0xC00D5212) nas três.
    ///
    /// Um arquivo, e não uma câmera, porque nenhuma câmera desta máquina abre
    /// pelo Media Foundation — e a negociação do leitor é a mesma para os dois.
    /// Cada quadrante tem uma cor, então o teste prova também a orientação e a
    /// ordem dos canais, e não só que "chegou um quadro".
    #[test]
    fn a_source_without_nv12_is_negotiated_and_read_the_right_way_up() {
        let Ok(_session) = Session::new() else {
            eprintln!("sem Media Foundation nesta maquina; nada a provar aqui");
            return;
        };
        let directory = std::env::temp_dir().join(format!("ldktela-mf-{}", std::process::id()));
        let size = Size {
            width: 640,
            height: 480,
        };

        // Três níveis de folga, medidos: RGB e YUY2 saem exatos, e o MJPEG, que
        // tem perda, erra por 2. Um erro de faixa de cor (0–255 no lugar de
        // 16–235) passa de 20 no branco, e é justamente o que se quer pegar.
        const TOLERANCE: u8 = 3;
        for layout in [Layout::Rgb24, Layout::Yuy2, Layout::Mjpeg] {
            let path = write_avi(&directory, layout, size, 3);
            let Some(reader) = reader_over(&path) else {
                return;
            };

            let negotiated = negotiate(&reader, HD, 30)
                .unwrap_or_else(|error| panic!("{layout:?}: a negociacao falhou: {error}"));
            assert_eq!(negotiated.size, size, "{layout:?}: tamanho negociado");
            println!("{layout:?} -> {:?}", negotiated.pixels);

            let sample = first_sample(&reader);
            let mut converter = Converter::new(negotiated.pixels);
            let mut nv12 = NV12Buffer::new(size.width, size.height);
            assert!(
                read_frame(&sample, &negotiated, &mut converter, &mut nv12),
                "{layout:?}: o quadro nao foi lido"
            );

            let (stride_y, _) = nv12.strides();
            let (luma, _) = nv12.data_mut();
            // O centro de cada quadrante, longe das bordas onde o JPEG e a
            // subamostragem de croma borram a cor.
            let centres = [
                (size.width / 4, size.height / 4),
                (size.width * 3 / 4, size.height / 4),
                (size.width / 4, size.height * 3 / 4),
                (size.width * 3 / 4, size.height * 3 / 4),
            ];
            for ((x, y), expected) in centres.into_iter().zip(expected_luma()) {
                let got = luma[y as usize * stride_y as usize + x as usize];
                println!("  {layout:?} ({x}, {y}): Y={got}, esperado {expected}");
                assert!(
                    got.abs_diff(expected) <= TOLERANCE,
                    "{layout:?}: em ({x}, {y}) esperava Y={expected}, veio {got} — \
                     imagem de cabeca para baixo, canais trocados ou faixa de cor errada"
                );
            }
        }
    }
}
