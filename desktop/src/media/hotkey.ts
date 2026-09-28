import type { ShareChoice } from './session';
import type { ShareSource } from './native';

/**
 * O que o atalho global compartilha quando aperta a tecla e nada está no ar.
 *
 * A tela sempre vem antes das janelas na lista do core (`capture.rs`), então a
 * primeira fonte do tipo `screen` é sempre a que o próprio core rotulou "Tela
 * 1" — não há por que reordenar aqui. Sem monitor nenhum enumerado (capturador
 * indisponível), não há o que oferecer, e quem chama decide o que fazer com
 * `null`.
 */
export function defaultShareChoice(sources: readonly ShareSource[]): ShareChoice | null {
  const screen = sources.find((source) => source.kind === 'screen');
  if (screen === undefined) {
    return null;
  }
  return {
    sourceId: screen.id,
    kind: screen.kind,
    // O atalho existe para não abrir o seletor: começa com áudio, que é o que
    // se quer na maioria das vezes, e quem quiser sem som troca depois.
    audio: true,
    title: screen.title,
  };
}
