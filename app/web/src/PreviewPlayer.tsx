/**
 * Preview ao vivo de UMA fonte listada (modal Compartilhar).
 *
 * - Roda só com `active` (modal aberto + app em foco + fonte com id real).
 *   Sem Tauri (mock/navegador): placeholder honesto, nenhum invoke.
 * - Frames GLP2/format-0 chegam no Channel e desenham no canvas com o
 *   mesmo renderer do player (sem acks: latest-only, best-effort).
 * - Erro do backend (permissão, fonte sumida, câmera ocupada) vira nota
 *   via `onError`; o thumb estático do modal continua valendo.
 * - O token sobe via `onToken` para o dono parar antes de compartilhar
 *   (a mesma câmera não abre duas vezes).
 */

import { useEffect, useRef, useState } from "react";
import { deliverPlayerFrame } from "./playerFrameDelivery";
import {
  Channel,
  isTauri,
  previewStart,
  previewStop,
  selfviewStart,
  selfviewStop,
  type PreviewKind,
} from "./api";
import { createPlayerRenderer, parsePlayerFrame } from "./playerRenderer";

/**
 * App em foco agora? Puro e testável: foco da janela E visibilidade do
 * documento precisam ser verdade (blur ou aba oculta pausam o preview).
 */
export function appFocusNow(
  windowFocused: boolean,
  documentHidden: boolean,
): boolean {
  return windowFocused && !documentHidden;
}

/**
 * Assenta o unlisten do `onFocusChanged`: se o efeito já desmontou
 * (`cancelled`), para na hora em vez de guardar um `off` que ninguém
 * chamaria — sem isso o listener vaza após o unmount. Puro e testável.
 */
export function settleFocusUnlisten(
  cancelled: boolean,
  stop: () => void,
  onLive: (stop: () => void) => void,
): void {
  if (cancelled) stop();
  else onLive(stop);
}

/**
 * Foco da janela do app. Fora do Tauri (mock/testes/SSR) assume focado e
 * delega a visibilidade ao documento — nunca quebra o caminho real.
 */
export function useAppFocus(): boolean {
  const [focused, setFocused] = useState(true);
  useEffect(() => {
    if (!isTauri()) return;
    let off: (() => void) | undefined;
    let cancelled = false;
    void import("@tauri-apps/api/window")
      .then((win) => {
        if (cancelled) return;
        const current = win.getCurrentWindow();
        current.isFocused().then(
          (initial) => {
            if (!cancelled) setFocused(initial);
          },
          () => undefined,
        );
        current.onFocusChanged(({ payload }) => {
          if (!cancelled) setFocused(payload);
        }).then(
          (stop) => settleFocusUnlisten(cancelled, stop, (live) => { off = live; }),
          () => undefined,
        );
      })
      .catch(() => undefined);
    const onVisibility = (): void => {
      if (document.hidden) setFocused(false);
      else {
        void import("@tauri-apps/api/window")
          .then((win) => win.getCurrentWindow().isFocused())
          .then(
            (initial) => {
              if (!cancelled) setFocused(initial);
            },
            () => undefined,
          );
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", onVisibility);
      off?.();
    };
  }, []);
  return focused;
}

export interface PreviewPlayerProps {
  kind: PreviewKind;
  id: string;
  /** Modal aberto + app em foco + fonte válida (o dono decide). */
  active: boolean;
  onToken?: (token: string | null) => void;
  onError?: (message: string | null) => void;
  /**
   * True enquanto a abertura está em voo (dispositivo abrindo). O dono
   * desabilita o Compartilhar nesse intervalo: a mesma câmera não abre
   * duas vezes, e clicar no meio da abertura falharia ocupado.
   */
  onPending?: (pending: boolean) => void;
}

export function PreviewPlayer({ kind, id, active, onToken, onError, onPending }: PreviewPlayerProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [frames, setFrames] = useState(0);
  const tokenCb = useRef(onToken);
  tokenCb.current = onToken;
  const errorCb = useRef(onError);
  errorCb.current = onError;
  const pendingCb = useRef(onPending);
  pendingCb.current = onPending;

  useEffect(() => {
    if (!active || !isTauri()) return;
    let disposed = false;
    let token: string | null = null;
    let renderer: ReturnType<typeof createPlayerRenderer> | undefined;
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (buffer) => {
      if (disposed) return;
      let frame: ReturnType<typeof parsePlayerFrame>;
      try {
        frame = parsePlayerFrame(buffer);
      } catch {
        return;
      }
      try {
        deliverPlayerFrame(
          () => {
            const target = canvas.current;
            if (!target) throw new Error("canvas unavailable");
            renderer ??= createPlayerRenderer(target);
            renderer.draw(frame);
            setFrames((n) => n + 1);
          },
          () => undefined,
        );
      } catch {
        // Desenho falhou (canvas sumiu): o próximo frame tenta de novo.
      }
    };
    pendingCb.current?.(true);
    void (async () => {
      try {
        token = await previewStart(kind, id, channel);
        if (disposed) {
          await previewStop(token).catch(() => undefined);
          return;
        }
        tokenCb.current?.(token);
        errorCb.current?.(null);
      } catch (failure) {
        if (!disposed) {
          errorCb.current?.(
            failure instanceof Error ? failure.message : "Preview indisponível.",
          );
        }
      } finally {
        pendingCb.current?.(false);
      }
    })();
    return () => {
      disposed = true;
      pendingCb.current?.(false);
      renderer?.dispose();
      tokenCb.current?.(null);
      if (token) void previewStop(token).catch(() => undefined);
    };
  }, [kind, id, active]);

  if (!isTauri()) {
    return (
      <p className="hint" data-testid="preview-mock">
        Preview ao vivo só no app (aqui vale o thumb estático).
      </p>
    );
  }
  if (!active) return null;
  return (
    <div className="preview-live" data-testid="preview-live">
      <canvas ref={canvas} aria-label={`Pré-visualização de ${kind}`} />
      {frames === 0 ? <span className="watch-note">Aguardando preview…</span> : null}
    </div>
  );
}

/**
 * Renderer do player reaproveitado para os previews locais.
 */
export type PreviewRenderer = ReturnType<typeof createPlayerRenderer>;

/**
 * Desenha UM frame GLP2 no canvas (renderer reaproveitado entre frames).
 * Puro o bastante para teste: o canvas entra pronto, o renderer sai junto.
 */
export function drawPreviewFrame(
  canvas: HTMLCanvasElement,
  renderer: PreviewRenderer | undefined,
  buffer: ArrayBuffer,
  onFirstFrame: () => void,
): { renderer: PreviewRenderer; drew: boolean } {
  const frame = parsePlayerFrame(buffer);
  let active = renderer;
  let drew = false;
  deliverPlayerFrame(
    () => {
      active ??= createPlayerRenderer(canvas);
      active.draw(frame);
      drew = true;
      onFirstFrame();
    },
    () => undefined,
  );
  if (!active) throw new Error("canvas unavailable");
  return { renderer: active, drew };
}

export interface SelfViewPlayerProps {
  /** Share no ar + pref visível + app em foco (o dono decide). */
  active: boolean;
  nickname: string;
  onToken?: (token: string | null) => void;
  onError?: (message: string | null) => void;
}

/**
 * Tile "Você": espelha o feed do share ativo (sem reabrir dispositivo).
 * Mock: placeholder honesto. Sem share no backend: nota verbatim.
 */
export function SelfViewPlayer({ active, nickname, onToken, onError }: SelfViewPlayerProps) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const [frames, setFrames] = useState(0);
  const tokenCb = useRef(onToken);
  tokenCb.current = onToken;
  const errorCb = useRef(onError);
  errorCb.current = onError;

  useEffect(() => {
    if (!active || !isTauri()) return;
    let disposed = false;
    let token: string | null = null;
    let renderer: PreviewRenderer | undefined;
    const channel = new Channel<ArrayBuffer>();
    channel.onmessage = (buffer) => {
      if (disposed) return;
      try {
        const out = drawPreviewFrame(
          canvas.current ?? (() => { throw new Error("canvas unavailable"); })(),
          renderer,
          buffer,
          () => {
            if (!disposed) setFrames((n) => n + 1);
          },
        );
        renderer = out.renderer;
      } catch {
        // Frame inválido ou canvas sumiu: o próximo tenta de novo.
      }
    };
    void (async () => {
      try {
        token = await selfviewStart(channel);
        if (disposed) {
          await selfviewStop(token).catch(() => undefined);
          return;
        }
        tokenCb.current?.(token);
        errorCb.current?.(null);
      } catch (failure) {
        if (!disposed) {
          errorCb.current?.(
            failure instanceof Error ? failure.message : "Prévia local indisponível.",
          );
        }
      }
    })();
    return () => {
      disposed = true;
      renderer?.dispose();
      tokenCb.current?.(null);
      if (token) void selfviewStop(token).catch(() => undefined);
    };
  }, [active]);

  if (!isTauri()) {
    return (
      <p className="hint" data-testid="selfview-mock">
        Prévia local só no app (aqui vale o palco dos outros).
      </p>
    );
  }
  if (!active) return null;
  return (
    <div className="preview-live selfview" data-testid="selfview-live">
      <canvas ref={canvas} aria-label={`Seu vídeo (${nickname})`} />
      {frames === 0 ? <span className="watch-note">Aguardando seu vídeo…</span> : null}
    </div>
  );
}
