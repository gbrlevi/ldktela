# ADR-0039 — A câmera tem dois caminhos de captura: Media Foundation e DirectShow

- **Status:** Aceito; emendado em 2026-09-27 (ver o fim)
- **Data:** 2026-09-23
- **Altera:** a decisão 8 do [ADR-0038](0038-camera-e-uma-segunda-publicacao.md), que dizia
  "a captura é nossa, em Media Foundation". Continua sendo nossa; deixa de ser só em Media
  Foundation. O resto do 0038 fica intacto.

## Contexto

O 0038 escolheu Media Foundation e escreveu o porquê no cabeçalho do módulo: é a API que o
Windows mantém viva, é ela que responde pela configuração de privacidade da câmera, é o
frame server que deixa dois aplicativos lerem um dispositivo, e é dela que vêm os
conversores de formato que nos pouparam de escrever um decodificador. Nada disso ficou
falso.

O que ficou falso foi a premissa de que **toda câmera fala Media Foundation**.

Na primeira vez que o recurso encontrou uma máquina de usuário, as câmeras não abriram.
O quadro medido, contra o DroidCam desta máquina:

| O que foi tentado | Resultado |
|---|---|
| `MFEnumDeviceSources` | encontra os dispositivos, com nome e link simbólico |
| `MFCreateDeviceSource` pelo link simbólico | `E_INVALIDARG` (`0x80070057`) |
| `IMFActivate::ActivateObject`, o caminho canônico | `E_INVALIDARG` |
| Em thread MTA limpa, com `MFStartup` novo | `E_INVALIDARG` |
| Categorias `VIDEO_CAMERA` e `CAPTURE` | `E_INVALIDARG` |
| Aplicativo **Câmera** do Windows, que é só MF | erro; nenhuma câmera oferecida |

Ao mesmo tempo, as mesmas câmeras entregam vídeo no Google Meet, e o Meet lista **quatro**
onde o Media Foundation enumera **duas** — as que faltam incluem a OBS Virtual Camera, que
é um filtro DirectShow puro.

A conclusão é que essas câmeras **existem para o Media Foundation e não abrem por ele**.
São câmeras virtuais: DroidCam, OBS, Iriun, EpocCam, ManyCam. Elas registram um filtro
DirectShow, e algumas registram também um dispositivo KS que a enumeração do MF encontra
mas cuja ativação falha. O Chromium não tem esse problema porque implementa os dois
caminhos e cai para o DirectShow quando o primeiro não serve.

**Isso não é caso de borda para este produto.** O público é de quem já transmite: OBS e
câmera de celular são o normal, não a exceção. Uma câmera que funciona no Discord e no Meet
e não funciona aqui é, para quem usa, um defeito nosso — e a única razão de a câmera estar
no escopo ([ADR-0038](0038-camera-e-uma-segunda-publicacao.md)) é fazer melhor do que o
Discord faz.

## Decisão

1. **Dois caminhos de captura, com o Media Foundation na frente.** O MF continua sendo o
   preferido por tudo o que o 0038 listou. O DirectShow é o caminho alternativo, e existe
   para os dispositivos que o MF não abre.

2. **A lista de câmeras é uma só, unificada.** Os dois caminhos enumeram, e o resultado é
   fundido: um dispositivo visto pelos dois aparece **uma vez**. Expor "DroidCam (MF)" e
   "DroidCam (DirectShow)" seria vazar a nossa implementação para dentro de um menu em que a
   pessoa só quer escolher um rosto.

3. **A fusão casa por caminho de dispositivo, depois por nome.** O nome de exibição de um
   moniker DirectShow de dispositivo físico contém o mesmo link simbólico que o MF usa, e é
   ele que casa os dois. Câmeras virtuais sem caminho de dispositivo casam pelo nome
   amigável, que para elas é distintivo.

4. **A escolha do caminho acontece ao abrir, não ao listar.** Tentar abrir cada dispositivo
   durante a enumeração para descobrir quem responde custaria segundos e ligaria o LED de
   todas as câmeras da máquina toda vez que o menu abrisse. Então a lista é otimista: ela
   mostra o dispositivo, e o caminho se resolve quando ele é escolhido.

5. **A queda para o DirectShow é automática e silenciosa.** Quando o MF recusa um
   dispositivo que ele mesmo acabou de enumerar, tentamos o DirectShow com o dispositivo
   correspondente antes de reportar erro. Quem escolheu uma câmera quer a câmera, não um
   aviso sobre qual API do Windows a abriu. Se os dois recusarem, o erro reportado é o **do
   Media Foundation**: ele é o caminho preferido, e o código dele é o que se pesquisa.

6. **O destino do grafo DirectShow é um filtro nosso**, implementando `IBaseFilter`, `IPin`
   e `IMemInputPin`, e não o `ISampleGrabber`. O sample grabber seria muito menor, mas está
   aposentado: a Microsoft o tirou do SDK, ele não existe nos bindings que usamos, e ele
   mora no `qedit.dll`, que faz parte do Media Feature Pack e **pode não estar na máquina**.
   Uma câmera que falha em Windows N por falta de uma DLL aposentada é exatamente o defeito
   que este ADR existe para não repetir. O Chromium escreveu o próprio filtro pela mesma
   razão.

7. **A conversão de cor é nossa, porque o DirectShow não tem o conversor do MF.** O caminho
   MF pede NV12 e recebe NV12, com o `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING` inserindo o
   que faltar. No DirectShow negociamos o formato **que o dispositivo já emite** e
   convertemos aqui: NV12, I420/IYUV, YUY2, UYVY, RGB32 e RGB24.

8. **Formato comprimido não entra.** Se o dispositivo só oferecer MJPEG ou H.264,
   recusamos com mensagem própria em vez de deixar o DirectShow montar um decodificador
   por conta. Um grafo montado por adivinhação insere filtros de terceiros no nosso
   processo — o conector "inteligente" do DirectShow é conhecido por isso — e o custo de
   depurar o que ele escolheu supera o de não suportar um dispositivo raro. Toda câmera que
   emite MJPEG emite também YUY2 em alguma resolução.

9. **O caminho DirectShow é empurrado, e não puxado.** O MF bloqueia em `ReadSample` numa
   thread nossa; o DirectShow chama `IMemInputPin::Receive` na thread de streaming do
   próprio dispositivo. Os dois terminam no mesmo lugar — um `Frames` que converte, escala
   e entrega ao encoder —, e é esse ponto de encontro que mantém a diferença contida num
   módulo só.

10. **Um dispositivo perdido é evento, não silêncio.** O MF descobre pela flag de fim de
    fluxo; o DirectShow, pelo `EC_DEVICE_LOST` na fila de eventos do grafo. Os dois chamam
    o mesmo `on_lost`, e quem compartilha vê a transmissão terminar com motivo.

## Consequências

- Uma câmera virtual que só fala DirectShow passa a funcionar, que é o objetivo.
- O módulo `camera.rs` vira o diretório `camera/`, com `mf.rs`, `dshow.rs` e `convert.rs`
  em volta de uma API pública que não mudou. Continua inteiro na zona de revisão humana
  (`CLAUDE.md` §10).
- Ganhamos ~700 linhas de COM inseguro, das quais a maior parte é um filtro DirectShow que
  precisa estar certo em detalhes que não aparecem em teste sem hardware: contagem de
  referência entre filtro e pino, quem é dono de qual `AM_MEDIA_TYPE`, e o ciclo que o pino
  não pode criar com o grafo. É a parte deste ADR que mais pode dar errado, e é por isso que
  o teste contra a câmera de verdade — `#[ignore]`, como o teste de SFU real — é parte da
  entrega e não um extra.
- A configuração de privacidade de câmera do Windows não governa o caminho DirectShow como
  governa o MF. Quem bloqueia a câmera no Windows e mesmo assim a vê funcionar por uma
  câmera virtual não está furando a nossa porta: está usando um filtro de software que
  nunca passou por aquela configuração, e isso vale igual para o Discord, o Meet e o OBS.
- Não passamos a suportar captura de áudio por DirectShow, nem qualquer outro uso de grafo.
  O que entra é uma câmera, com um filtro de destino, e nada além disso.

## Alternativas rejeitadas

- **Só Media Foundation, e documentar a limitação.** É o estado de hoje. Deixa de fora as
  câmeras que o público efetivamente usa, num recurso cuja razão de existir é ser melhor
  que a alternativa. Foi a premissa que a primeira máquina de usuário derrubou.
- **`ISampleGrabber` do `qedit.dll`.** Cerca de 60 linhas em vez de 600. Rejeitado pela
  decisão 6: interface aposentada, ausente dos bindings, e numa DLL que pode não existir.
- **Pedir que a pessoa configure a câmera virtual em "modo Media Foundation".** O DroidCam
  não tem esse modo; a OBS Virtual Camera não tem esse modo. Exigir configuração que não
  existe é recusar com passos extras.
- **Usar o `getUserMedia` do WebView e mandar os quadros ao Rust.** O Chromium resolveria a
  enumeração sozinho, e é tentador. Contraria o [ADR-0026](0026-publicacao-no-rust-nativo.md)
  — nada no WebView adquire mídia —, e passar vídeo cru por IPC a 30 fps é justamente o
  custo que aquele ADR existe para não pagar.

## Emenda de 2026-09-27 — o Media Foundation também negocia o formato nativo

### O que aconteceu

Um usuário relatou, já com o motivo no aviso: *"o Windows recusou a camera: pedindo NV12 a camera:
Nenhuma transformação adequada foi encontrada (0xC00D5212)"*. É `MF_E_TOPO_CODEC_NOT_FOUND`, e o
defeito era a premissa da decisão 7. O caminho Media Foundation pedia NV12 ao leitor e confiava em
`MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING` para produzi-lo. A documentação desse atributo diz o
contrário: ele faz **YUV → RGB-32 e desentrelaçamento, e nada mais**. Nunca produziu NV12 de coisa
alguma. Funcionava só em câmera que já emite NV12, como a integrada de notebook — por isso o
recurso passou nos primeiros testes e falhou nas câmeras dos usuários.

A sequência explicava também o relato anterior dos usuários de DroidCam: a câmera **abria** pelo
Media Foundation, então o DirectShow nunca era tentado; a trilha era publicada; e só então, já na
thread de captura, a negociação falhava — ladrilho preto para a sala e "a câmera foi encerrada"
para quem transmitia.

O defeito foi reproduzido nesta máquina, com a mesma mensagem e o mesmo `HRESULT`, passando o
código antigo por um leitor aberto sobre arquivos AVI em RGB24 e em YUY2. O leitor de fonte é o
mesmo para câmera e para arquivo, e é nele que a negociação acontece.

### Decisão

1. **O Media Foundation escolhe um formato nativo da câmera e o fixa no dispositivo.** Pedir só a
   saída deixava o leitor escolher a entrada sozinho. A ordem é a mesma regra de tamanho e taxa
   do DirectShow, com os formatos sem compressão na frente dos comprimidos e, entre eles, o mais
   barato de converter. Se o melhor tamanho só existe comprimido, o melhor formato sem compressão
   fica de reserva, logo atrás.

2. **A conversão é nossa nos dois caminhos.** NV12, I420, YUY2, UYVY, RGB32 e RGB24 chegam como a
   câmera os emite e passam por `convert.rs`, que agora recebe o passo de linha e a orientação
   explicitamente, porque o Media Foundation entrega o passo que o driver alocou e diz a
   orientação pelo sinal dele. Isto altera a decisão 7: o Windows só **decodifica**.

3. **Formato comprimido passa por um decodificador, pedido pelo que sabemos ler.** Para MJPEG e
   H.264, pedimos ao leitor NV12, YUY2, I420 ou RGB32, nessa ordem, no tamanho e na taxa do
   formato nativo. O leitor é criado com `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`, que
   insere um processador de vídeo de verdade, e a saída pede **faixa limitada** (16–235)
   explicitamente: JPEG é faixa cheia, e sem o pedido o conversor a mantém.

4. **Abrir e negociar acontecem antes de publicar, nos dois caminhos.** O Media Foundation abre e
   negocia na própria thread e responde por canal antes de `start` retornar, como o DirectShow já
   fazia. E a câmera abre antes de a trilha existir: uma câmera que não entrega quadro falha no
   botão, e a sala nunca a vê.

5. **Qualquer recusa do Media Foundation cai para o DirectShow**, exceto "ocupada" e "bloqueada
   pelo Windows". Isto amplia a decisão 5, que só caía quando a câmera não abria. Uma câmera que
   abre e não oferece nada conversível é, na prática, o mesmo caso.

6. **Quando os dois caminhos recusam, o erro traz os dois motivos.** Isto altera a outra metade
   da decisão 5, que mostrava só o do Media Foundation. Numa máquina de usuário, o texto do aviso
   é todo o diagnóstico que existe.

### O que os testes pegaram no caminho

O teste que passa AVIs de verdade pelo leitor do Media Foundation — RGB24 de baixo para cima,
YUY2 e MJPEG, com um quadrante de cor em cada canto — pegou dois defeitos que nenhum teste de
unidade pegaria:

- **Os GUIDs de RGB são outros no Media Foundation.** `MEDIASUBTYPE_RGB24` (`e436eb7d-…`) é do
  DirectShow; o Media Foundation usa `MFVideoFormat_RGB24` (`00000014-…`). Reconhecendo só o
  primeiro, toda câmera RGB parecia comprimida para o caminho MF.
- **O MJPEG saía em faixa cheia**: branco com Y=255 onde deveria ser 235. Toda webcam MJPEG
  teria transmitido com o branco estourado e o preto esmagado.

O teste roda no `just check`. Se a máquina não tem Media Foundation ou o leitor de AVI dele, ele
avisa e pula; qualquer falha de negociação ou de leitura reprova.

### O que continua sem prova

Nenhuma câmera física desta máquina abre pelo Media Foundation. O caminho foi provado com
arquivos, que passam pelo mesmo leitor, e pelo DirectShow com as câmeras reais. A prova com a
câmera de quem relatou o defeito só vem com a versão nas mãos dele.
