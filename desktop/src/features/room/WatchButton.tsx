import { sourceLabel, type PublicationSource } from '../../media/publication';
import { Icon } from '../../ui/Icon';

interface WatchButtonProps {
  /** De qual publicação da pessoa este botão entra ou sai (ADR-0038). */
  source: PublicationSource;
  /** Se esta publicação está sendo assistida agora. */
  watching: boolean;
  onClick: () => void;
  /**
   * `md` na tela de entrada, onde este é o único gesto de cada linha; `sm` na
   * lista do popover, que já é densa. O desenho é o mesmo, e é isso que importa:
   * o mesmo gesto tem a mesma cara nos dois lugares.
   */
  size?: 'sm' | 'md';
}

/**
 * Entrar ou sair da tela ou da câmera de alguém (ADR-0036, ADR-0038).
 *
 * Uma pílula, e não um `Button`: numa linha de lista o botão cheio pesava mais
 * que o nome da pessoa e brigava com a etiqueta ao lado. Entrar é o gesto que
 * convida, então leva o destaque; sair é o que se faz sem cerimônia, então fica
 * discreto até o ponteiro chegar.
 *
 * O ícone é o da fonte, e não um olho: é ele que diz *do que* se entra ou sai, e
 * isso deixa a pílula caber na linha do nome. Com uma sub-linha por fonte, quem
 * transmitia só uma coisa ganhava uma linha a mais para dizer "Tela" ao lado de
 * um botão — e a lista ficava desencontrada entre quem transmite e quem não.
 */
export function WatchButton({ source, watching, onClick, size = 'sm' }: WatchButtonProps) {
  const action = watching ? 'Sair' : 'Entrar';
  const target = source === 'camera' ? 'câmera' : 'tela';
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={watching ? `Sair da ${target}` : `Entrar na ${target}`}
      title={sourceLabel(source)}
      className={`flex shrink-0 items-center rounded-pill font-medium ${
        size === 'md' ? 'gap-1.5 px-3 py-1 text-sm' : 'gap-1 px-2 py-0.5 text-xs'
      } ${
        watching
          ? 'text-text-muted hover:bg-surface-3 hover:text-text'
          : 'bg-accent-soft text-text hover:bg-accent/30'
      }`}
    >
      <Icon name={source === 'camera' ? 'camera' : 'monitor'} size={size === 'md' ? 16 : 14} />
      {action}
    </button>
  );
}
