/**
 * Mock automático para navegador puro (sem Tauri).
 *
 * Ativo quando `isTauriMissing()` é true — ou seja, fora da casca Tauri
 * (`window.__TAURI__` / `window.__TAURI_INTERNALS__` ausentes). Serve só para
 * acertar o visual lobby -> sala sem backend; o caminho Tauri real não muda.
 *
 * Formatos espelham `api.ts` (OwnerSnapshot, RoomMember, SourceInfo,
 * CapabilitySet, MediaCounters, EffectiveQuality).
 */

import type {
  CapabilitySet,
  EffectiveQuality,
  MediaCounters,
  OwnerSnapshot,
  RoomMember,
  SourceInfo,
} from "./api";

/** Código de exemplo exibido no placeholder do lobby. */
export const MOCK_ROOM_CODE = "K7Q9XA";

/** Id estável do "Você" no mock. */
export const MOCK_SELF_ID = "m-voce";

/**
 * true no navegador puro (sem Tauri). Em SSR/testes (sem window) devolve
 * false para nunca sequestrar o caminho real nem os testes.
 */
export function isTauriMissing(): boolean {
  if (typeof window === "undefined") return false;
  const w = window as unknown as Record<string, unknown>;
  return !(w.__TAURI__ || w.__TAURI_INTERNALS__);
}

/** Gera um código de sala mock (6 chars, sem ambíguos). */
export function randomMockCode(): string {
  const alphabet = "ABCDEFGHJKMNPQRSTUVWXYZ23456789";
  let out = "";
  const pick = (): number => Math.floor(Math.random() * alphabet.length);
  for (let i = 0; i < 6; i++) out += alphabet[pick()];
  return out;
}

/** Roster mock: Você (líder) + Ana + Bruno + Carla. */
export function mockRoster(selfNickname: string, sharing: boolean): RoomMember[] {
  const self = selfNickname.trim() || "Você";
  return [
    { id: MOCK_SELF_ID, nickname: self, master: true, share: sharing },
    { id: "m-ana", nickname: "Ana", master: false, share: true },
    { id: "m-bruno", nickname: "Bruno", master: false, share: false },
    { id: "m-carla", nickname: "Carla", master: false, share: true },
  ];
}

/** Snapshot live mock (session open; share live/stopped conforme `sharing`). */
export function mockSnapshot(sharing: boolean, watching: string[]): OwnerSnapshot {
  return {
    session: { id: "sess:mock", state: "open" },
    share: { id: sharing ? "share:mock" : null, state: sharing ? "live" : "stopped" },
    links: watching.map((watcher) => ({
      id: `link:${watcher}`,
      watcher,
      state: "connected" as const,
    })),
    watchers: [...watching],
    roster: [
      { member: MOCK_SELF_ID, watcher: watching.includes(MOCK_SELF_ID) },
      { member: "m-ana", watcher: watching.includes("m-ana") },
      { member: "m-bruno", watcher: watching.includes("m-bruno") },
      { member: "m-carla", watcher: watching.includes("m-carla") },
    ],
  };
}

/** Efetivo autoritativo mock (720p medium, geração 1). */
export function mockEffective(): EffectiveQuality {
  return {
    profile: { w: 1280, h: 720, bitrate_kbps: 2000, fps: 30 },
    generation: 1,
  };
}

/** Contadores/links mock (um LinkStats por membro assistido). */
export function mockCounters(
  watching: string[],
  effective: EffectiveQuality | null,
): MediaCounters {
  const titles: Record<string, string> = {
    [MOCK_SELF_ID]: "Você",
    "m-ana": "Ana",
    "m-bruno": "Bruno",
    "m-carla": "Carla",
  };
  return {
    connected: true,
    frames: 1240,
    keyframes: 12,
    keyframes_seen: true,
    presented: 1218,
    links: watching.map((member) => ({
      member,
      title: titles[member] ?? member,
      codec: "H.264 Constrained Baseline",
      width: 1280,
      height: 720,
      decoded: 1240,
      presented: 1218,
      dropped: 22,
      render_fps: 30,
      bitrate_bps: 2_000_000,
      bitrate_note: "mock local (navegador puro, sem backend)",
      delay_estimate_ms: null,
      delay_note: "mock: sem RTT real",
      dropped_note: "mock: decodificados menos apresentados",
    })),
    effective,
    backend: "videotoolbox",
    backend_note: null,
  };
}

/** Fontes mock (1 display + 1 window + 1 webcam). */
export function mockSources(): SourceInfo[] {
  return [
    { kind: "display", id: "0", name: "Tela principal · 1920×1080", w: 1920, h: 1080 },
    { kind: "window", id: "42", name: "VS Code · goDrinking", w: 1280, h: 800 },
    { kind: "camera", id: "0", name: "Webcam · 1280×720", w: 1280, h: 720 },
  ];
}

/** Caps mock (display/window/camera supported). */
export function mockCaps(): CapabilitySet {
  return {
    display: { supported: true, reason: "mock (navegador)" },
    window: { supported: true, reason: "mock (navegador)" },
    camera: { supported: true, reason: "mock (navegador)" },
    app_audio: { supported: false, reason: "mock: sem áudio mapeado" },
    exclusion: { supported: false, reason: "mock: sem exclusão" },
  };
}
