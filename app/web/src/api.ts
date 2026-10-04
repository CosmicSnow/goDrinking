/**
 * Único lugar que nomeia comandos e eventos Tauri.
 *
 * Assinaturas copiadas de `app/src/lib.rs` (não adivinhe — leia lá antes de
 * mudar qualquer nome aqui):
 *
 * - create_room {nickname, password} -> string (código da sala)
 * - join_room {code, nickname, password} -> string (nosso member id)
 * - leave {} -> ()
 * - start_share {source, w?, h?, bitrateKbps?, fps?} -> ()
 *   ("synthetic", "movie:/caminho", "display:<id>", "window:<id>",
 *   "camera:<id>", "combo:display:<id>+camera:<cid>" ou
 *   "combo:window:<id>+camera:<cid>" = tela + webcam no canto, um feed só);
 *   perfil opcional (senão 720p30)
 * - stop_share {} -> ()
 * - set_quality {w, h, bitrate_kbps, fps, preset?} -> {profile, generation}
 *   (top-level, sem wrapper; preset "low"|"medium"|"high" é hint de display,
 *   os números mandam; erros redatados: "qualidade: …", "not sharing")
 * - watch {member} -> ()
 * - unwatch {member} -> ()
 * - get_snapshot {} -> OwnerSnapshot
 * - get_roster {} -> RoomMember[] (pull explícito do roster guardado;
 *   mesmos dados do evento "roster" — para a UI que montou após o emit)
 * - get_media_counters {} -> MediaCounters (links + effective autoritativo)
 * - set_server {base} -> string (base normalizada)
 * - list_sources {} -> SourceInfo[] | source_capabilities {} -> CapabilitySet
 *   (fontes de captura; e2e_* são test-only, fora do caminho da UI)
 * - list_audio_apps {} -> AudioApp[]
 * - set_audio_exclusions {apps} -> ()
 * - preview_source {kind, id} -> {data_url, w, h} (thumb PNG lazy ~256px;
 *   data_url null quando indisponível — nunca quebra a listagem)
 *
 * Perfil efetivo autoritativo: `get_media_counters().effective` (None fora
 * do share) + evento `media-event {kind:"quality", profile, generation}`
 * (a geração só chega async). A UI nunca trata o staged como efetivo.
 *
 * Eventos de `app/src/pump.rs` (cargas já redigidas no backend: kinds e
 * contagens — nunca SDP, candidates ou tokens):
 * - "signal-event": admitted | pending | roster | watch | unwatch |
 *   signal | kicked | gone
 * - "media-event": ice-connected | ice-failed | frame | keyframe | stats |
 *   gathering-complete | error
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Snapshot (core/src/owner.rs + core/src/state.rs). Estados serializam em
// minúsculas ("open", "live", "negotiating", …); ids são strings opacas.
// ---------------------------------------------------------------------------

export type SalaState = "closed" | "joining" | "open" | "closing";
export type ShareState = "stopped" | "starting" | "live" | "stopping";
export type LinkState = "absent" | "negotiating" | "connected" | "closing";

export interface OwnerSnapshot {
  session: { id: string | null; state: SalaState };
  share: { id: string | null; state: ShareState };
  links: Array<{ id: string; watcher: string; state: LinkState }>;
  /** Observadores com link vivo (derivado dos links, nunca do roster). */
  watchers: string[];
  /** Roster do owner: só membro + flag de watcher (sem apelido). */
  roster: Array<{ member: string; watcher: boolean }>;
}

/** Membro do roster vindo do evento "roster" (com apelido, master e share). */
export interface RoomMember {
  id: string;
  nickname: string;
  master: boolean;
  share: boolean;
}

// ---------------------------------------------------------------------------
// Eventos Tauri (app/src/pump.rs).
// ---------------------------------------------------------------------------

export type SignalEvent =
  | { kind: "admitted" }
  | { kind: "pending" }
  | { kind: "roster"; entries: RoomMember[]; master: string | null }
  | { kind: "watch"; from: string }
  | { kind: "unwatch"; from: string }
  | { kind: "signal"; from: string; type: string }
  | { kind: "kicked" }
  | { kind: "gone" };

export type MediaEvent =
  | { kind: "ice-connected" }
  | { kind: "ice-failed" }
  | { kind: "frame"; non_black: boolean; motion: boolean }
  | { kind: "keyframe" }
  | {
      kind: "stats";
      frames: number;
      keyframes: number;
      ice: boolean;
      host: number;
      srflx: number;
      /** Frames presented on the in-room / pop-up surface (acks). Omitted by old shells. */
      presented?: number;
      /** Per-link (lado viewer; host recebe []). Omitted by old shells. */
      links?: LinkStats[];
      /** Codificador vivo no host; null até o build. Omitted by old shells. */
      backend?: string | null;
      /** Motivo do fallback; null no hardware. Omitted by old shells. */
      backend_note?: string | null;
    }
  | { kind: "gathering-complete" }
  | { kind: "quality"; profile: QualityProfile; generation: number }
  | { kind: "error" };

export interface ViewerStats {
  frames: number;
  keyframes: number;
  ice: boolean;
  /** Frames presented on the in-room / pop-up surface (distinct from decoded). */
  presented: number;
}

// ---------------------------------------------------------------------------
// Intents. Uma função por ação do usuário; chamadas só de handlers ou de
// callbacks de evento — nunca de timers.
// ---------------------------------------------------------------------------

/** Cria a sala. Retorna o código (não é segredo). */
export function createRoom(nickname: string, password: string): Promise<string> {
  return invoke<string>("create_room", { nickname, password });
}

/** Entra na sala. Retorna nosso member id (não é segredo). */
export function joinRoom(
  code: string,
  nickname: string,
  password: string,
): Promise<string> {
  return invoke<string>("join_room", { code, nickname, password });
}

/** Sai da sala (idempotente). */
export function leaveRoom(): Promise<void> {
  return invoke<void>("leave");
}

/** Inicia o share. Perfil opcional: sem ele o backend usa 720p30. */
export function startShare(source: string, profile?: QualityProfile): Promise<void> {
  if (!profile) {
    return invoke<void>("start_share", { source });
  }
  const { w, h, bitrate_kbps, fps } = profile;
  return invoke<void>("start_share", {
    source,
    w,
    h,
    bitrateKbps: bitrate_kbps,
    fps,
  });
}

/** Para o share (links e captura liberados no backend). */
export function stopShare(): Promise<void> {
  return invoke<void>("stop_share");
}

/**
 * Aplica o perfil ao share vivo. O macro Tauri converte parâmetros Rust
 * snake_case para chaves IPC camelCase por padrão (`argument_case = Camel`),
 * então `bitrate_kbps` viaja como `bitrateKbps` — conversão feita AQUI, na
 * fronteira do invoke, e em nenhum outro lugar.
 */
export function setQuality(
  profile: QualityProfile,
  preset?: QualityPresetHint,
): Promise<EffectiveQuality> {
  const { w, h, bitrate_kbps, fps } = profile;
  return invoke<EffectiveQuality>("set_quality", {
    w,
    h,
    bitrateKbps: bitrate_kbps,
    fps,
    ...(preset ? { preset } : {}),
  });
}

/** Pede para assistir ao membro (intenção de watch, lado viewer). */
export function watchMember(member: string): Promise<void> {
  return invoke<void>("watch", { member });
}

/** Cancela o watch e derruba a mídia do viewer. */
export function unwatchMember(member: string): Promise<void> {
  return invoke<void>("unwatch", { member });
}

/** Leitura observacional. Nunca avança lifecycle. */
export function getSnapshot(): Promise<OwnerSnapshot> {
  return invoke<OwnerSnapshot>("get_snapshot");
}

/**
 * Pull explícito do roster guardado no backend (app/src/lib.rs
 * `get_roster`: mesmo mapeamento RoomMember do evento "roster" do pump).
 * Chamado só de `refresh()` — nunca de timers. Vazio fora da sala.
 */
export function getRoster(): Promise<RoomMember[]> {
  return invoke<RoomMember[]>("get_roster");
}

/**
 * Estatística por link (app/src/video.rs `LinkStats`, serializada como está).
 * `delay_estimate_ms` é sempre None até o core expor RTT (BLOQUEADO) — a UI
 * mostra "—" honesto + `delay_note`. `dropped` é aproximação (decodificados
 * menos apresentados); `bitrate_bps` é medido pós-decode (ver `bitrate_note`).
 */
export interface LinkStats {
  member: string;
  title: string;
  codec: string;
  width: number;
  height: number;
  decoded: number;
  presented: number;
  dropped: number;
  render_fps: number;
  bitrate_bps: number;
  bitrate_note: string;
  delay_estimate_ms: number | null;
  delay_note: string;
  dropped_note: string;
}

/**
 * Perfil de qualidade validado (core/src/media.rs `QualityProfile`,
 * serializado como está). `preset` só existe no comando, como hint.
 */
export interface QualityProfile {
  w: number;
  h: number;
  bitrate_kbps: number;
  fps: number;
}

/** Hint de display do comando; os números do perfil mandam. */
export type QualityPresetHint = "low" | "medium" | "high";

/**
 * Perfil efetivo autoritativo (app/src/lib.rs `EffectiveQuality`): último
 * perfil aceito (ou o default do start_share) + geração do fence. A geração
 * conta reconfigurações aplicadas e chega async via evento `quality`.
 */
export interface EffectiveQuality {
  profile: QualityProfile;
  generation: number;
}

/** Contadores de mídia observados no backend (pollable fallback). */
export interface MediaCounters {
  connected: boolean;
  frames: number;
  keyframes: number;
  keyframes_seen: boolean;
  presented: number;
  /** Um item por membro assistido (sala ou pop-up); vazio ocioso/host. */
  links: LinkStats[];
  /** Efetivo autoritativo; None fora do share. */
  effective: EffectiveQuality | null;
  /** Codificador vivo (`videotoolbox`/`nvenc`/`openh264`); null até o primeiro build. */
  backend: string | null;
  /** Motivo do fallback software; null no hardware ou sem backend. */
  backend_note: string | null;
}

export function getMediaCounters(): Promise<MediaCounters> {
  return invoke<MediaCounters>("get_media_counters");
}

/** Define a base do rendezvous. Retorna a base normalizada. */
export function setServer(base: string): Promise<string> {
  return invoke<string>("set_server", { base });
}

// ---------------------------------------------------------------------------
// Fontes de captura (platform/). Tipos espelham o backend; ids são opacos.
// ---------------------------------------------------------------------------

/** Uma fonte capturável listada pelo backend. */
export interface SourceInfo {
  kind: "display" | "window" | "camera";
  id: string;
  name: string;
  w: number;
  h: number;
  /** Thumb PNG (data URL) quando o App já buscou; ausente = gradiente. */
  thumbnail?: string | null;
}

/** Thumb one-shot do backend (sempre resolve; null = indisponível). */
export interface SourcePreview {
  data_url: string | null;
  w: number;
  h: number;
}

export interface Support {
  supported: boolean;
  reason: string;
}

/** Capacidades de captura desta build (UI desabilita com o motivo). */
export interface CapabilitySet {
  display: Support;
  window: Support;
  /**
   * Webcam (opcional: backends antigos não emitem — a UI trata ausente
   * como desconhecido e libera a opção; o backend valida de verdade).
   */
  camera?: Support;
  app_audio: Support;
  exclusion: Support;
}

/** Lista displays/janelas. Pode pedir permissão ao SO no primeiro uso. */
export function listSources(): Promise<SourceInfo[]> {
  return invoke<SourceInfo[]>("list_sources");
}

/**
 * Thumb one-shot de uma fonte (lazy, sob demanda do modal). Resolve sempre:
 * `data_url` null quando indisponível (sem permissão, fonte sumiu) — uma
 * fonte sem thumb nunca quebra as outras.
 */
export function previewSource(kind: string, id: string): Promise<SourcePreview> {
  return invoke<SourcePreview>("preview_source", { kind, id });
}

/** Fonte pré-visualizável ao vivo (modal Compartilhar). */
export type PreviewKind = "display" | "window" | "camera";

/**
 * Inicia o preview ao vivo de uma fonte listada. Devolve um token opaco;
 * frames GLP2/format-0 chegam no `channel` até `previewStop(token)`.
 * Erra tipado (permissão, fonte sumida, câmera ocupada) — nunca silencioso.
 */
export function previewStart(
  kind: PreviewKind,
  id: string,
  channel: import("@tauri-apps/api/core").Channel<ArrayBuffer>,
): Promise<string> {
  return invoke<string>("preview_start", { kind, id, channel });
}

/** Para um preview ao vivo. Idempotente; token desconhecido é Ok. */
export function previewStop(token: string): Promise<void> {
  return invoke<void>("preview_stop", { token });
}

/**
 * Inicia a prévia local do share ATIVO (tile "Você" no palco). Espelha o
 * feed do bridge — sem segunda abertura de dispositivo. Devolve token;
 * frames GLP2/format-1 chegam no `channel` até `selfviewStop(token)`.
 * Erra honesto sem share ("inicie o compartilhamento…").
 */
export function selfviewStart(
  channel: import("@tauri-apps/api/core").Channel<ArrayBuffer>,
): Promise<string> {
  return invoke<string>("selfview_start", { channel });
}

/** Para a prévia local. Idempotente; token desconhecido é Ok. */
export function selfviewStop(token: string): Promise<void> {
  return invoke<void>("selfview_stop", { token });
}

/** Capacidades sem tocar no SO (nunca pede permissão). */
export function sourceCapabilities(): Promise<CapabilitySet> {
  return invoke<CapabilitySet>("source_capabilities");
}

export interface AudioApp {
  name: string;
  id: string;
  pid: number;
  emitting_audio: boolean;
}

export function listAudioApps(): Promise<AudioApp[]> {
  return invoke<AudioApp[]>("list_audio_apps");
}

export function setAudioExclusions(apps: string[]): Promise<void> {
  return invoke<void>("set_audio_exclusions", { apps });
}

// ---------------------------------------------------------------------------
// Autodireção test-only (app/src/lib.rs: E2ePlan).
//
// `get_e2e_plan` devolve o plano somente quando o app foi lançado com
// `--e2e-plan '<json>'`. Sem plano, estes comandos recusam tudo e o app
// segue o caminho normal — modo e2e impossível na UI comum.
// ---------------------------------------------------------------------------

/** Plano de autodireção (só existe com `--e2e-plan`). */
export interface E2ePlan {
  role: "host" | "viewer";
  server: string;
  password: string;
  nickname: string;
  code_file: string;
  status_file: string;
  /** Fonte do share ("synthetic" quando ausente; "movie:<path>" | "display:<id>"). */
  share?: string;
  quality?: QualityProfile;
}

/** Devolve o plano ativo ou null (app normal). */
export function getE2ePlan(): Promise<E2ePlan | null> {
  return invoke<E2ePlan | null>("get_e2e_plan");
}

/** Escreve o payload JSON no status_file do plano (recusa segredos). */
export function e2eStatus(payload: unknown): Promise<void> {
  return invoke<void>("e2e_status", { payload: JSON.stringify(payload) });
}

/** Lê o room code publicado pelo host (erra até o host publicar). */
export function e2eReadCode(): Promise<string> {
  return invoke<string>("e2e_read_code");
}

// ---------------------------------------------------------------------------
// Escuta dos eventos (com cleanup; sem polling).
// ---------------------------------------------------------------------------

export function onSignalEvent(cb: (payload: SignalEvent) => void): Promise<UnlistenFn> {
  return listen<SignalEvent>("signal-event", (event) => cb(event.payload));
}

export function onMediaEvent(cb: (payload: MediaEvent) => void): Promise<UnlistenFn> {
  return listen<MediaEvent>("media-event", (event) => cb(event.payload));
}

// Binary presentation channel: sequence/width/height (u32 LE), then RGBA.
export interface PlayerState {
  member: string;
  title: string;
  popup: boolean;
  volume: number;
  muted: boolean;
  mute_all: boolean;
}
export { Channel, isTauri } from "@tauri-apps/api/core";
export const playerAttach = (member: string, token: string, channel: import("@tauri-apps/api/core").Channel<ArrayBuffer>) =>
  invoke<PlayerState>("player_attach", { member, token, channel });
export const playerDetach = (member: string, token: string) => invoke<void>("player_detach", { member, token });
export const playerAck = (member: string, token: string, seq: number, drawn: boolean, drawUs?: number, gpu?: boolean) => invoke<void>("player_ack", { member, token, seq, drawn, drawUs, gpu });
export const playerContext = () => invoke<PlayerState>("player_context");
export const playerPopup = (member: string, popup: boolean) => invoke<void>("player_popup", { member, popup });
export const playerAudio = (member: string, volume: number, muted: boolean) => invoke<void>("player_audio", { member, volume, muted });
export const playerMuteAll = (muted: boolean) => invoke<void>("player_mute_all", { muted });
export const onPlayerState = (cb: (state: PlayerState) => void) => listen<PlayerState>("player-state", e => cb(e.payload));
export const onPlayerEnded = (cb: (member: string) => void) => listen<string>("player-ended", e => cb(e.payload));
