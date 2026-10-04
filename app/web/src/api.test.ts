// Intents chamam o comando exato de app/src/lib.rs (nome + args).
// Eventos escutam os nomes exatos de app/src/pump.rs.
import { beforeEach, describe, expect, it, vi } from "vitest";

const { mockInvoke, mockListen } = vi.hoisted(() => ({
  mockInvoke: vi.fn(),
  mockListen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: mockInvoke,
  Channel: class Channel {},
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: mockListen }));

import {
  createRoom,
  getMediaCounters,
  getRoster,
  getSnapshot,
  joinRoom,
  leaveRoom,
  listAudioApps,
  listSources,
  onMediaEvent,
  onSignalEvent,
  previewSource,
  previewStart,
  previewStop,
  selfviewStart,
  selfviewStop,
  setAudioExclusions,
  setQuality,
  setServer,
  sourceCapabilities,
  startShare,
  stopShare,
  unwatchMember,
  watchMember,
  playerAck,
  playerAudio,
  playerMuteAll,
  playerPopup,
} from "./api";

beforeEach(() => {
  mockInvoke.mockReset().mockResolvedValue(null);
  mockListen.mockReset().mockResolvedValue(() => undefined);
});

describe("intents (comando certo, args certos)", () => {
  it("create_room leva nickname + password e devolve o código", async () => {
    mockInvoke.mockResolvedValueOnce("ABC123");
    await expect(createRoom("Ana", "segredo")).resolves.toBe("ABC123");
    expect(mockInvoke).toHaveBeenCalledWith("create_room", {
      nickname: "Ana",
      password: "segredo",
    });
  });

  it("join_room leva code + nickname + password e devolve o member id", async () => {
    mockInvoke.mockResolvedValueOnce("m-9");
    await expect(joinRoom("ABC123", "Bia", "segredo")).resolves.toBe("m-9");
    expect(mockInvoke).toHaveBeenCalledWith("join_room", {
      code: "ABC123",
      nickname: "Bia",
      password: "segredo",
    });
  });

  it("leave não leva args", async () => {
    await leaveRoom();
    expect(mockInvoke).toHaveBeenCalledWith("leave");
  });

  it("set_quality leva o payload top-level exato (sem wrapper)", async () => {
    const result = {
      profile: { w: 1280, h: 720, bitrate_kbps: 2000, fps: 30 },
      generation: 0,
    };
    mockInvoke.mockResolvedValueOnce(result);
    await expect(
      setQuality({ w: 1280, h: 720, bitrate_kbps: 2000, fps: 30 }, "medium"),
    ).resolves.toEqual(result);
    // Tauri converte snake_case Rust para camelCase no IPC (argument_case
    // default = Camel): a fronteira do invoke envia bitrateKbps.
    expect(mockInvoke).toHaveBeenCalledWith("set_quality", {
      w: 1280,
      h: 720,
      bitrateKbps: 2000,
      fps: 30,
      preset: "medium",
    });
  });

  it("set_quality custom omite o preset", async () => {
    const result = {
      profile: { w: 640, h: 360, bitrate_kbps: 1000, fps: 24 },
      generation: 0,
    };
    mockInvoke.mockResolvedValueOnce(result);
    await expect(
      setQuality({ w: 640, h: 360, bitrate_kbps: 1000, fps: 24 }),
    ).resolves.toEqual(result);
    expect(mockInvoke).toHaveBeenCalledWith("set_quality", {
      w: 640,
      h: 360,
      bitrateKbps: 1000,
      fps: 24,
    });
  });

  it("start_share leva a fonte opaca; stop_share não leva args", async () => {
    await startShare("synthetic");
    expect(mockInvoke).toHaveBeenCalledWith("start_share", { source: "synthetic" });
    await startShare("synthetic", { w: 1920, h: 1080, bitrate_kbps: 10000, fps: 60 });
    expect(mockInvoke).toHaveBeenCalledWith("start_share", {
      source: "synthetic",
      w: 1920,
      h: 1080,
      bitrateKbps: 10000,
      fps: 60,
    });
    await startShare("movie:/tmp/a.mp4");
    expect(mockInvoke).toHaveBeenCalledWith("start_share", { source: "movie:/tmp/a.mp4" });
    await stopShare();
    expect(mockInvoke).toHaveBeenCalledWith("stop_share");
  });

  it("watch/unwatch endereçam o membro", async () => {
    await watchMember("m-1");
    expect(mockInvoke).toHaveBeenCalledWith("watch", { member: "m-1" });
    await unwatchMember("m-1");
    expect(mockInvoke).toHaveBeenCalledWith("unwatch", { member: "m-1" });
  });

  it("get_snapshot é leitura sem args; set_server leva a base", async () => {    mockInvoke.mockResolvedValueOnce({ session: { id: null, state: "open" } });
    await getSnapshot();
    expect(mockInvoke).toHaveBeenCalledWith("get_snapshot");
    mockInvoke.mockResolvedValueOnce("http://127.0.0.1:18790");
    await expect(setServer("http://127.0.0.1:18790/")).resolves.toBe(
      "http://127.0.0.1:18790",
    );
    expect(mockInvoke).toHaveBeenCalledWith("set_server", {
      base: "http://127.0.0.1:18790/",
    });
  });

  it("get_roster sem args devolve os membros no formato do evento roster", async () => {
    const roster = [
      { id: "m-1", nickname: "Convidado", master: true, share: false },
      { id: "m-2", nickname: "Ana", master: false, share: true },
    ];
    mockInvoke.mockResolvedValueOnce(roster);
    await expect(getRoster()).resolves.toEqual(roster);
    expect(mockInvoke).toHaveBeenCalledWith("get_roster");
  });

  it("preview_source leva kind+id e devolve o thumb (null quando indisponível)", async () => {
    const preview = { data_url: "data:image/png;base64,iVBOR", w: 256, h: 144 };
    mockInvoke.mockResolvedValueOnce(preview);
    await expect(previewSource("display", "1")).resolves.toEqual(preview);
    expect(mockInvoke).toHaveBeenCalledWith("preview_source", { kind: "display", id: "1" });
    mockInvoke.mockResolvedValueOnce({ data_url: null, w: 0, h: 0 });
    await expect(previewSource("window", "42")).resolves.toEqual({
      data_url: null,
      w: 0,
      h: 0,
    });
  });

  it("preview_start leva kind+id+channel e devolve o token; preview_stop leva o token", async () => {
    const channel = { onmessage: null };
    mockInvoke.mockResolvedValueOnce("pv-1-2");
    await expect(
      previewStart("camera", "0", channel as never),
    ).resolves.toBe("pv-1-2");
    expect(mockInvoke).toHaveBeenCalledWith("preview_start", {
      kind: "camera",
      id: "0",
      channel,
    });
    mockInvoke.mockResolvedValueOnce(undefined);
    await previewStop("pv-1-2");
    expect(mockInvoke).toHaveBeenCalledWith("preview_stop", { token: "pv-1-2" });
  });

  it("selfview_start leva só o channel e devolve o token; selfview_stop leva o token", async () => {
    const channel = { onmessage: null };
    mockInvoke.mockResolvedValueOnce("sv-1-2");
    await expect(selfviewStart(channel as never)).resolves.toBe("sv-1-2");
    expect(mockInvoke).toHaveBeenCalledWith("selfview_start", { channel });
    mockInvoke.mockResolvedValueOnce(undefined);
    await selfviewStop("sv-1-2");
    expect(mockInvoke).toHaveBeenCalledWith("selfview_stop", { token: "sv-1-2" });
  });
});

describe("eventos (nomes exatos do backend)", () => {
  it("escuta signal-event e entrega o payload", async () => {
    type Handler = (event: { payload: unknown }) => void;
    const handlers = new Map<string, Handler>();
    mockListen.mockImplementation((event: string, cb: Handler) => {
      handlers.set(event, cb);
      return Promise.resolve(() => undefined);
    });
    const seen: unknown[] = [];
    await onSignalEvent((payload) => seen.push(payload));
    expect(mockListen).toHaveBeenCalledWith("signal-event", expect.any(Function));
    handlers.get("signal-event")?.({ payload: { kind: "roster", entries: [], master: null } });
    expect(seen).toEqual([{ kind: "roster", entries: [], master: null }]);
  });

  it("escuta media-event e entrega o payload", async () => {
    type Handler = (event: { payload: unknown }) => void;
    const handlers = new Map<string, Handler>();
    mockListen.mockImplementation((event: string, cb: Handler) => {
      handlers.set(event, cb);
      return Promise.resolve(() => undefined);
    });
    const seen: unknown[] = [];
    await onMediaEvent((payload) => seen.push(payload));
    expect(mockListen).toHaveBeenCalledWith("media-event", expect.any(Function));
    handlers.get("media-event")?.({
      payload: { kind: "stats", frames: 30, keyframes: 2, ice: true, host: 1, srflx: 0 },
    });
    expect(seen).toEqual([
      { kind: "stats", frames: 30, keyframes: 2, ice: true, host: 1, srflx: 0 },
    ]);
  });
});

describe("fontes de captura (nomes exatos do backend)", () => {
  it("list_sources sem args devolve a lista opaca", async () => {
    const listed = [
      { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
    ];
    mockInvoke.mockResolvedValueOnce(listed);
    await expect(listSources()).resolves.toEqual(listed);
    expect(mockInvoke).toHaveBeenCalledWith("list_sources");
  });

  it("source_capabilities sem args devolve suporte com motivos", async () => {
    const caps = {
      display: { supported: true, reason: "ScreenCaptureKit" },
      window: { supported: true, reason: "ScreenCaptureKit" },
      app_audio: { supported: false, reason: "planejado (lane de áudio)" },
      exclusion: { supported: true, reason: "SCContentFilter" },
    };
    mockInvoke.mockResolvedValueOnce(caps);
    await expect(sourceCapabilities()).resolves.toEqual(caps);
    expect(mockInvoke).toHaveBeenCalledWith("source_capabilities");
  });

  it("list_audio_apps sem args devolve apps opacos", async () => {
    const apps = [{ name: "Discord", id: "com.hnc.Discord", pid: 42, emitting_audio: true }];
    mockInvoke.mockResolvedValueOnce(apps);
    await expect(listAudioApps()).resolves.toEqual(apps);
    expect(mockInvoke).toHaveBeenCalledWith("list_audio_apps");
  });

  it("set_audio_exclusions leva os tokens", async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await setAudioExclusions(["com.hnc.Discord"]);
    expect(mockInvoke).toHaveBeenCalledWith("set_audio_exclusions", { apps: ["com.hnc.Discord"] });
  });
});

describe("contadores de mídia (fallback observacional com links)", () => {
  it("get_media_counters sem args devolve links por membro intactos", async () => {
    const counters = {
      connected: true,
      frames: 120,
      keyframes: 4,
      keyframes_seen: true,
      presented: 118,
      links: [
        {
          member: "m-2",
          title: "Bia",
          codec: "H.264 Constrained Baseline",
          width: 1280,
          height: 720,
          decoded: 120,
          presented: 118,
          dropped: 2,
          render_fps: 29.7,
          bitrate_bps: 1800000,
          bitrate_note: "medido em bytes RGBA apresentados (pos-decode)",
          delay_estimate_ms: null,
          delay_note: "estimativa indisponivel: RTT do par ICE nao exposto pelo core",
          dropped_note: "aproximacao: decodificados menos apresentados",
        },
      ],
    };
    mockInvoke.mockResolvedValueOnce(counters);
    await expect(getMediaCounters()).resolves.toEqual(counters);
    expect(mockInvoke).toHaveBeenCalledWith("get_media_counters");
  });
});

describe("player inline (comando certo, args certos)", () => {
  it("player_popup move só o membro pedido", async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await playerPopup("m-2", true);
    expect(mockInvoke).toHaveBeenCalledWith("player_popup", { member: "m-2", popup: true });
  });

  it("player_audio e mute-all levam ganho 0–1", async () => {
    mockInvoke.mockResolvedValue(undefined);
    await playerAudio("m-2", 0.4, false);
    expect(mockInvoke).toHaveBeenCalledWith("player_audio", { member: "m-2", volume: 0.4, muted: false });
    await playerMuteAll(true);
    expect(mockInvoke).toHaveBeenCalledWith("player_mute_all", { muted: true });
  });

  it("player_ack confirma o frame desenhado", async () => {
    mockInvoke.mockResolvedValueOnce(undefined);
    await playerAck("m-2", "tok", 9, true);
    expect(mockInvoke).toHaveBeenCalledWith("player_ack", { member: "m-2", token: "tok", seq: 9, drawn: true });
  });
});
