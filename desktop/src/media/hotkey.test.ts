import { describe, expect, it } from 'vitest';
import { defaultShareChoice } from './hotkey';
import type { ShareSource } from './native';

function source(kind: ShareSource['kind'], id: string, title: string): ShareSource {
  return { id, kind, title };
}

describe('defaultShareChoice', () => {
  it('picks the first screen, with audio on, when there is one', () => {
    const sources = [
      source('window', 'hwnd-1', 'Discord'),
      source('screen', 'monitor-1', 'Tela 1'),
      source('screen', 'monitor-2', 'Tela 2'),
    ];

    expect(defaultShareChoice(sources)).toEqual({
      sourceId: 'monitor-1',
      kind: 'screen',
      audio: true,
      title: 'Tela 1',
    });
  });

  it('returns null when no screen was enumerated', () => {
    const sources = [source('window', 'hwnd-1', 'Discord')];

    expect(defaultShareChoice(sources)).toBeNull();
  });

  it('returns null with no sources at all', () => {
    expect(defaultShareChoice([])).toBeNull();
  });
});
