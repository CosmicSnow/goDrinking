// Render de snapshot sem timers nem DOM: props entram, markup estático sai.
// Estados vazios sempre trazem diagnóstico — nunca área muda silenciosa.
import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";
import {
  HomeScreen,
  QUALITY_DISABLED_REASON,
  QUALITY_PRESETS,
  QualityPanel,
  RoomScreen,
  SELFVIEW_STORAGE_KEY,
  WINDOW_HINT,
  DEFAULT_AUDIO_EXCLUSION_TOKENS,
  NICKNAME_STORAGE_KEY,
  SERVER_STORAGE_KEY,
  audioAppMatchesToken,
  defaultExcludedAppIds,
  formatBps,
  formatDelayMs,
  formatFps,
  frameRefreshDue,
  linkLabel,
  readSelfviewPref,
  readStoredSetting,
  resolveDesired,
  salaLabel,
  shareIntentFromResolved,
  shareLabel,
  sourceKindOf,
  watchingStillLive,
  visibleAudioApps,
  writeSelfviewPref,
  writeStoredSetting,
  isSelf,
  roomTilesClassName,
  stageCellClassName,
  stageMembers,
  tileIsLive,
  validateBitrate,
  validateCode,
  validateCustomDim,
  validateFps,
  validateNickname,
  validatePassword,
  validateSource,
  type QualityPanelProps,
} from "./views";
import type { AudioApp, LinkStats, OwnerSnapshot, RoomMember } from "./api";

const noop = (..._args: unknown[]): void => undefined;

const qualityFixture = (overrides: Partial<QualityPanelProps> = {}): QualityPanelProps => ({
  shareLive: true,
  busy: false,
  effective: null,
  backend: null,
  backendNote: null,
  applying: false,
  applyError: null,
  resolution: "720p",
  onResolution: noop,
  customW: "",
  onCustomW: noop,
  customH: "",
  onCustomH: noop,
  quality: "medium",
  onQuality: noop,
  customBitrate: "",
  onCustomBitrate: noop,
  customFps: "",
  onCustomFps: noop,
  srcDims: null,
  onApply: noop,
  ...overrides,
});

const linkFixture = (overrides: Partial<LinkStats> = {}): LinkStats => ({
  member: "m-2",
  title: "Bia",
  codec: "H.264 Constrained Baseline",
  width: 1280,
  height: 720,
  decoded: 120,
  presented: 118,
  dropped: 2,
  render_fps: 29.7,
  bitrate_bps: 1_800_000,
  bitrate_note: "medido em bytes RGBA apresentados (pos-decode)",
  delay_estimate_ms: null,
  delay_note: "estimativa indisponivel: RTT do par ICE nao exposto pelo core",
  dropped_note: "aproximacao: decodificados menos apresentados",
  ...overrides,
});

const snapshotFixture = (overrides: Partial<OwnerSnapshot> = {}): OwnerSnapshot => ({
  session: { id: "sess:1", state: "open" },
  share: { id: null, state: "stopped" },
  links: [],
  watchers: [],
  roster: [],
  ...overrides,
});

const roomProps = (overrides: Partial<Parameters<typeof RoomScreen>[0]> = {}) => ({
  roomCode: "ABC123",
  nickname: "Ana",
  snapshot: snapshotFixture(),
  roster: [] as RoomMember[],
  selfId: "m-1",
  selfNickname: "Ana",
  watching: [] as string[],
  source: "synthetic",
  onSource: noop,
  sources: [],
  sourcesError: null as string | null,
  caps: null,
  busy: false,
  error: null as string | null,
  lastSignal: null as string | null,
  lastMedia: null as string | null,
  stats: null,
  quality: qualityFixture(),
  linkStats: null as LinkStats[] | null,
  onListSources: noop,
  onRefresh: noop,
  onLeave: noop,
  onShare: noop,
  onStopShare: noop,
  onWatch: noop,
  onUnwatch: noop,
  ...overrides,
});

describe("validação (regras do backend, sem segredos nas mensagens)", () => {
  it("apelido 2–24, charset restrito", () => {
    expect(validateNickname("A")).not.toBeNull();
    expect(validateNickname("Ana")).toBeNull();
    expect(validateNickname("a".repeat(25))).not.toBeNull();
    expect(validateNickname("a<b")).not.toBeNull();
  });

  it("senha 4–64 obrigatória", () => {
    expect(validatePassword("123")).not.toBeNull();
    expect(validatePassword("segredo")).toBeNull();
  });

  it("código e fonte", () => {
    expect(validateCode("AB")).not.toBeNull();
    expect(validateCode("ABC123")).toBeNull();
    expect(validateSource("synthetic")).toBeNull();
    expect(validateSource("movie:/tmp/a.mp4")).toBeNull();
    expect(validateSource("display:1")).toBeNull();
    expect(validateSource("window:42")).toBeNull();
    expect(validateSource("screen")).not.toBeNull();
    expect(validateSource("movie:")).not.toBeNull();
    expect(validateSource("display:")).not.toBeNull();
    expect(validateSource("window:")).not.toBeNull();
  });
});

describe("persistência da home (localStorage, sem quebrar sem window)", () => {
  it("chaves estáveis de nick + servidor", () => {
    expect(NICKNAME_STORAGE_KEY).toBe("golive.nickname");
    expect(SERVER_STORAGE_KEY).toBe("golive.server");
  });

  it("sem window: leitura null, escrita sem throw", () => {
    expect(typeof window).toBe("undefined");
    expect(readStoredSetting(NICKNAME_STORAGE_KEY)).toBeNull();
    expect(() => writeStoredSetting(NICKNAME_STORAGE_KEY, "Ana")).not.toThrow();
  });
});

describe("rótulos de estado (espelham core/src/state.rs)", () => {
  it("traduz os estados sem inventar", () => {
    expect(salaLabel("open")).toBe("Aberta");
    expect(shareLabel("live")).toBe("No ar");
    expect(linkLabel("negotiating")).toBe("Negociando");
    expect(linkLabel("connected")).toBe("Conectado");
  });

  it("AO VIVO só com ICE e frame apresentado", () => {
    expect(tileIsLive(false, 10)).toBe(false);
    expect(tileIsLive(true, 0)).toBe(false);
    expect(tileIsLive(true, 1)).toBe(true);
  });
});

describe("RoomScreen AO VIVO honesto", () => {
  const members: RoomMember[] = [
    { id: "m-1", nickname: "Ana", master: true, share: false },
    { id: "m-2", nickname: "Bia", master: false, share: true },
  ];

  it("ICE connected sem frame apresentado não mostra AO VIVO", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          roster: members,
          watching: ["m-2"],
          snapshot: snapshotFixture({
            share: { id: "share:1", state: "live" },
            links: [{ id: "link:1", watcher: "m-2", state: "connected" }],
            watchers: ["m-2"],
          }),
          linkStats: [linkFixture({ member: "m-2", presented: 0 })],
        }),
      ),
    );
    expect(html).not.toContain("AO VIVO");
    expect(html).toContain("Aguardando vídeo");
  });

  it("contadores de outra superfície não anunciam AO VIVO antes do canvas desenhar", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          roster: members,
          watching: ["m-2"],
          snapshot: snapshotFixture({
            share: { id: "share:1", state: "live" },
            links: [{ id: "link:1", watcher: "m-2", state: "connected" }],
            watchers: ["m-2"],
          }),
          linkStats: [linkFixture({ member: "m-2", presented: 12 })],
        }),
      ),
    );
    expect(html).not.toContain("AO VIVO");
    expect(html).toContain("<canvas");
    expect(html).toContain("Volume de Bia");
    expect(html).toContain("Pop-up");
    expect(html).not.toContain("janela nativa");
  });

  it("pin deixa o tile no centro e os outros na mesma ordem (fila CSS)", () => {
    expect(roomTilesClassName(false)).toBe("tiles room-tiles");
    expect(roomTilesClassName(true)).toBe("tiles room-tiles focused");
    expect(["a", "b", "c"].map((id) => stageCellClassName(id, "b"))).toEqual([
      "stage-cell",
      "stage-cell primary-cell",
      "stage-cell",
    ]);
  });
});

describe("HomeScreen", () => {
  it("renderiza criar/entrar sem vazar a senha de volta", () => {
    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        server: "http://127.0.0.1:18790",
        onServer: noop,
        tab: "join",
        onTab: noop,
        nickname: "Ana",
        onNickname: noop,
        password: "segredo-super",
        onPassword: noop,
        code: "ABC123",
        onCode: noop,
        busy: false,
        error: null,
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain("Criar sala");
    expect(html).toContain("Entrar");
    expect(html).toContain('type="password"');
    // A senha digitada jamais volta como texto visível: só dentro do
    // próprio input mascarado (o navegador nunca a exibe).
    const visibleText = html.replace(/<input[^>]*>/g, "");
    expect(visibleText).not.toContain("segredo-super");
  });

  it("card de nome ligado ao apelido real", () => {
    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        nickname: "Jouy",
        onNickname: noop,
        password: "",
        onPassword: noop,
        code: "",
        onCode: noop,
        busy: false,
        error: null,
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain("Seu Nick (só pessoas na sala conseguem ver)");
    expect(html).toContain('id="nickname"');
    expect(html).toContain('value="Jouy"');
    expect(html).toContain("Como te chamam na sala");
  });

  it("card de servidor acima do nick, ligado às props legadas", () => {
    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        server: "https://together.jouymaker.com",
        defaultServer: "https://together.jouymaker.com",
        onServer: noop,
        nickname: "",
        onNickname: noop,
        password: "",
        onPassword: noop,
        code: "",
        onCode: noop,
        busy: false,
        error: null,
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain('id="server"');
    expect(html).toContain('value="https://together.jouymaker.com"');
    expect(html).not.toContain("Resetar URL");
    expect(html.indexOf('id="server"')).toBeLessThan(html.indexOf('id="nickname"'));
  });

  it("mostra Resetar URL só quando o servidor diverge do padrão", () => {
    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        server: "http://127.0.0.1:18790",
        defaultServer: "https://together.jouymaker.com",
        onServer: noop,
        nickname: "",
        onNickname: noop,
        password: "",
        onPassword: noop,
        code: "",
        onCode: noop,
        busy: false,
        error: null,
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain("Resetar URL");
  });

  it("mostra o erro sem área muda", () => {    const html = renderToStaticMarkup(
      createElement(HomeScreen, {
        server: "",
        onServer: noop,
        tab: "create",
        onTab: noop,
        nickname: "",
        onNickname: noop,
        password: "",
        onPassword: noop,
        code: "",
        onCode: noop,
        busy: false,
        error: "Senha: 4 a 64 caracteres (obrigatória).",
        onCreate: noop,
        onJoin: noop,
      }),
    );
    expect(html).toContain("Senha: 4 a 64 caracteres");
  });
});

describe("RoomScreen (snapshot → markup, sem inferência)", () => {
  const members: RoomMember[] = [
    { id: "m-1", nickname: "Ana", master: true, share: false },
    { id: "m-2", nickname: "Bia", master: false, share: true },
  ];

  it("roster com master/share, sem botão de watch para si", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ roster: members })));
    expect(html).toContain("ABC123");
    expect(html).toContain("♛");
    expect(html).toContain("compartilhando");
    expect(html).toContain("(você)");
    expect(html).toContain("Assistir");
    // Um botão Assistir (só Bia compartilha e não é você).
    expect(html.match(/Assistir/g)?.length).toBe(1);
  });

  it("topbar tem Copiar ao lado da pill; stage-head tem Atualizar, sem room-code-bar", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ roster: members })));
    expect(html).toContain("Copiar");
    expect(html).toContain("Atualizar");
    expect(html).toContain("Ignorar Áudio de Apps");
    expect(html).toContain("Disponível durante o compartilhamento de tela.");
    expect(html).not.toContain("room-code-bar");
    expect(html).not.toContain("Copiar código");
  });

  it("palco mostra só quem compartilha; sidebar lista todo mundo", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ roster: members })));
    // Ana (sem share) não tem tile; Bia (com share) tem.
    expect(html).not.toContain("tile-view-m-1");
    expect(html).toContain("tile-view-m-2");
    // Sidebar continua com todo mundo.
    expect(html).toContain("(você)");
  });

  it("palco omite o próprio share; só os outros entram no grid", () => {
    const bothShare: RoomMember[] = [
      { id: "m-1", nickname: "Ana", master: true, share: true },
      { id: "m-2", nickname: "Bia", master: false, share: true },
    ];
    expect(stageMembers(bothShare, "m-1", "Ana").map((m) => m.id)).toEqual(["m-2"]);
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ roster: bothShare })));
    expect(html).not.toContain("tile-view-m-1");
    expect(html).toContain("tile-view-m-2");
    expect(html).toContain("(você)");
    const alone = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({
        roster: [{ id: "m-1", nickname: "Ana", master: true, share: true }],
      })),
    );
    expect(alone).toContain("Sem transmissões");
    expect(alone).not.toContain("tile-view-m-1");
  });

  it("dois membros com o mesmo apelido: o palco usa o id, nunca o nick", () => {
    const twins: RoomMember[] = [
      { id: "m-host", nickname: "Ze", master: true, share: true },
      { id: "m-peer", nickname: "Ze", master: false, share: false },
    ];
    expect(isSelf(twins[0], "m-peer", "Ze")).toBe(false);
    expect(isSelf(twins[1], "m-peer", "Ze")).toBe(true);
    expect(stageMembers(twins, "m-peer", "Ze").map((m) => m.id)).toEqual(["m-host"]);
    expect(stageMembers(twins, "m-host", "Ze").map((m) => m.id)).toEqual([]);
    expect(stageMembers(twins, null, "Ze").map((m) => m.id)).toEqual(["m-host"]);
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({
        selfId: "m-peer",
        selfNickname: "Ze",
        roster: twins,
      })),
    );
    expect(html).toContain("tile-view-m-host");
    expect(html).not.toContain("tile-view-m-peer");
  });

  it("share live lista apps para ignorar áudio", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({ share: { id: "s1", state: "live" } }),
          audioApps: [
            { name: "Discord", id: "com.hnc.Discord", pid: 9, emitting_audio: true },
            { name: "Safari", id: "com.apple.Safari", pid: 8, emitting_audio: false },
          ],
          audioExcluded: ["com.hnc.Discord"],
        }),
      ),
    );
    expect(html).toContain("Discord");
    expect(html).toContain("Safari");
    expect(html).toContain("audio-list");
    expect(html).toContain("audio-card");
    expect(html).not.toContain("Disponível durante o compartilhamento de tela.");
  });

  it("lista de áudio junta o mesmo app e sobe os mutados", () => {
    const apps: AudioApp[] = [
      { name: "Safari", id: "com.apple.Safari", pid: 8, emitting_audio: false },
      { name: "Chrome", id: "com.google.Chrome", pid: 11, emitting_audio: false },
      { name: "Chrome", id: "com.google.Chrome", pid: 12, emitting_audio: true },
      { name: "Discord", id: "com.hnc.Discord", pid: 9, emitting_audio: true },
    ];
    const listed = visibleAudioApps(apps, ["com.google.Chrome"], "", false);
    expect(listed.map((app) => app.id)).toEqual(["com.google.Chrome", "com.hnc.Discord", "com.apple.Safari"]);
    expect(listed.filter((app) => app.id === "com.google.Chrome")).toHaveLength(1);
    expect(listed[0]?.emitting_audio).toBe(true);
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({ share: { id: "s1", state: "live" } }),
          audioApps: apps,
          audioExcluded: ["com.google.Chrome"],
        }),
      ),
    );
    const names = [...html.matchAll(/<b>([^<]+)<\/b>/g)].map((match) => match[1]);
    expect(names.filter((name) => name === "Chrome")).toHaveLength(1);
    expect(names.indexOf("Chrome")).toBeLessThan(names.indexOf("Discord"));
    expect(names.indexOf("Discord")).toBeLessThan(names.indexOf("Safari"));
  });

  it("matcher de exclusão espelha o backend (contains, case-insensitive)", () => {
    expect(audioAppMatchesToken("Discord", "com.hnc.Discord", "Discord")).toBe(true);
    expect(audioAppMatchesToken("Discord Helper (Renderer)", "x", "Discord")).toBe(true);
    expect(audioAppMatchesToken("Discord", "com.hnc.Discord.helper", "com.hnc.Discord")).toBe(true);
    expect(audioAppMatchesToken("discord", "COM.HNC.DISCORD", "Discord")).toBe(true);
    expect(audioAppMatchesToken("Discord", "com.hnc.Discord", "Discord.exe")).toBe(false);
    expect(audioAppMatchesToken("Safari", "com.apple.Safari", "Discord")).toBe(false);
    expect(audioAppMatchesToken("Discord", "com.hnc.Discord", "")).toBe(false);
    expect(audioAppMatchesToken("Discord", "com.hnc.Discord", "   ")).toBe(false);
  });

  it("defaults cobrem Discord + próprio app/helper (sync com platform)", () => {
    expect(DEFAULT_AUDIO_EXCLUSION_TOKENS).toContain("Discord");
    expect(DEFAULT_AUDIO_EXCLUSION_TOKENS).toContain("com.hnc.Discord");
    expect(DEFAULT_AUDIO_EXCLUSION_TOKENS).toContain("Discord.exe");
    expect(DEFAULT_AUDIO_EXCLUSION_TOKENS).toContain("dev.golive.sala");
    expect(DEFAULT_AUDIO_EXCLUSION_TOKENS).toContain("goDrinking.exe");
    expect(DEFAULT_AUDIO_EXCLUSION_TOKENS).toContain("golive-video.exe");
    const apps: AudioApp[] = [
      { name: "Discord", id: "com.hnc.Discord", pid: 9, emitting_audio: true },
      { name: "Safari", id: "com.apple.Safari", pid: 8, emitting_audio: false },
      { name: "goDrinking", id: "goDrinking.exe", pid: 7, emitting_audio: false },
      { name: "golive-video", id: "golive-video.exe", pid: 6, emitting_audio: false },
    ];
    expect(defaultExcludedAppIds(apps)).toEqual([
      "com.hnc.Discord",
      "goDrinking.exe",
      "golive-video.exe",
    ]);
  });

  it("defaults não engolem apps alheios nem duplicam ids", () => {
    const apps: AudioApp[] = [
      { name: "Safari", id: "com.apple.Safari", pid: 8, emitting_audio: false },
      { name: "Discord", id: "com.hnc.Discord", pid: 9, emitting_audio: true },
      { name: "Discord", id: "com.hnc.Discord", pid: 10, emitting_audio: false },
    ];
    expect(defaultExcludedAppIds(apps)).toEqual(["com.hnc.Discord"]);
  });

  it("ninguém compartilhando: palco vazio honesto, sidebar intacta", () => {
    const idle = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({ roster: [{ id: "m-9", nickname: "Zé", master: false, share: false }] }),
      ),
    );
    expect(idle).toContain("Sem transmissões");
    expect(idle).not.toContain("tile-view-m-9");
    expect(idle).toContain("Zé");
  });

  it("topo da sala não tem Compartilhar (fica no rodapé)", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect((html.match(/data-hook="share-open"/g) ?? []).length).toBe(1);
    expect(html).toContain("Sair da sala");
  });

  it("watching cai quando o host para de transmitir", () => {
    const entries: RoomMember[] = [
      { id: "m-1", nickname: "Ana", master: true, share: false },
      { id: "m-2", nickname: "Bia", master: false, share: true },
    ];
    expect(watchingStillLive(["m-1", "m-2"], entries)).toEqual(["m-2"]);
    expect(watchingStillLive(["m-1"], entries)).toEqual([]);
  });

  it("membro assistido mostra Parar de ver", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ roster: members, watching: ["m-2"] })),
    );
    expect(html).toContain("Parar de ver");
    expect(html).toContain("pedido de watch");
  });

  it("roster vazio traz diagnóstico, nunca lista muda", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(html).toContain("Nenhum membro visível ainda");
    expect(html).toContain("Atualizar");
  });

  it("links do snapshot com estado; sem links, diagnóstico", () => {
    const withLinks = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({
            share: { id: "share:1", state: "live" },
            links: [{ id: "link:1", watcher: "Bia", state: "negotiating" }],
            watchers: ["Bia"],
          }),
        }),
      ),
    );
    expect(withLinks).toContain("Bia");
    expect(withLinks).toContain("Negociando");
    // Share no ar: o botão de controle reflete o estado (selo removido).
    expect(withLinks).toContain("Parar de compartilhar");

    const empty = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(empty).toContain("Nenhum link ativo no snapshot");
  });

  it("placeholder de vídeo removido: stage sem selo de share nem contadores", () => {
    const withoutLink = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(withoutLink).not.toContain("video-placeholder");
    expect(withoutLink).not.toContain("Sem vídeo: nenhum link conectado");
    expect(withoutLink).not.toContain("share-state");
    expect(withoutLink).toContain("Sem transmissões");

    const connected = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({
            links: [{ id: "lnk:1", watcher: "m-2", state: "connected" }],
          }),
          stats: { frames: 120, keyframes: 4, ice: true, presented: 118 },
        }),
      ),
    );
    expect(connected).not.toContain("video-placeholder");
    expect(connected).not.toContain("Vídeo na janela nativa");
    expect(connected).not.toContain("Frames recebidos");
  });

  it("sem snapshot, diagnóstico pede Atualizar (nunca estado inventado)", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ snapshot: null })));
    expect(html).toContain("sem snapshot — toque Atualizar");
    expect(html).not.toContain("Aberta");
  });

  it("diagnóstico mostra último erro e últimos eventos", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({ error: "sala cheia", lastSignal: "roster (2 membro(s))", lastMedia: "stats (30 frames)" }),
      ),
    );
    expect(html).toContain("sala cheia");
    expect(html).toContain("roster (2 membro(s))");
    expect(html).toContain("stats (30 frames)");
  });
});

describe("fontes de captura (select + capacidades)", () => {
  it("sourceKindOf deriva o tipo do seletor opaco", () => {
    expect(sourceKindOf("synthetic")).toBe("synthetic");
    expect(sourceKindOf("movie:/a.mp4")).toBe("movie");
    expect(sourceKindOf("display:1")).toBe("display");
    expect(sourceKindOf("window:42")).toBe("window");
    expect(sourceKindOf("lixo")).toBe("synthetic");
  });

  it("display sem lista mostra o botão Listar + dica de permissão", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ source: "display:" })),
    );
    expect(html).toContain("Listar telas");
    expect(html).toContain("perm");
  });

  it("permissão negada mostra o bloco honesto com o caminho manual", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "display:",
          sourcesError: "Sem permissão de Gravação de Tela — autorize em Ajustes.",
          sourcesDenied: true,
        }),
      ),
    );
    expect(html).toContain("Sem permissão de Gravação de Tela");
    expect(html).toContain("Privacidade e Segurança");
    expect(html).toContain("Listar telas");
  });

  it("erro comum (sem denied) não mostra o caminho manual", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({ source: "display:", sourcesError: "Não foi listar as fontes." }),
      ),
    );
    expect(html).toContain("Não foi listar as fontes.");
    expect(html).not.toContain("Privacidade e Segurança");
  });

  it("capacidade negada desabilita a opção com o motivo", () => {
    const no = { supported: false, reason: "planejado (WGC/DXGI)" };
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "synthetic",
          caps: {
            display: no,
            window: no,
            app_audio: no,
            exclusion: no,
          },
        }),
      ),
    );
    expect(html).toContain("planejado (WGC/DXGI)");
  });

  it("lista populada oferece as fontes pelo nome", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "display:",
          sources: [
            { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
          ],
        }),
      ),
    );
    expect(html).toContain("Display 1 · 2560x1440");
  });

  it("modal Compartilhar: fonte selecionada destaca (.sel); confirmar é no botão", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "display:1",
          sources: [
            { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
          ],
        }),
      ),
    );
    // Seleção = highlight, sem compartilhar sozinho.
    expect(html).toContain('class="source sel"');
    // Confirmar/cancelar explícitos no rodapé do modal.
    expect(html).toContain("Cancelar");
    expect(html).toContain("Compartilhar");
  });

  it("thumb vira <img> quando há preview; sem preview, gradiente FONTE", () => {    const withThumb = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "display:",
          sources: [
            { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
          ],
          previews: { "display:1": "data:image/png;base64,iVBOR" },
        }),
      ),
    );
    expect(withThumb).toContain("<img");
    expect(withThumb).toContain("data:image/png;base64,iVBOR");

    const plain = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "display:",
          sources: [
            { kind: "display", id: "1", name: "Display 1 · 2560x1440", w: 2560, h: 1440 },
          ],
        }),
      ),
    );
    expect(plain).toContain("FONTE");
    expect(plain).not.toContain("data:image");
  });

  it("fonte com id real mostra o box de preview ao vivo (mock: placeholder)", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          source: "camera:0",
          sources: [
            { kind: "camera", id: "0", name: "Webcam · 1280×720", w: 1280, h: 720 },
          ],
        }),
      ),
    );
    expect(html).toContain("Pré-visualização ao vivo");
    expect(html).toContain("preview-mock");
  });

  it("sem fonte escolhida, sem box de preview", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ source: "synthetic" })),
    );
    expect(html).not.toContain("Pré-visualização ao vivo");
  });

  it("pref do self-view: visível por padrão, escrita sem window não quebra", () => {
    expect(typeof window).toBe("undefined");
    expect(readSelfviewPref()).toBe(true);
    expect(SELFVIEW_STORAGE_KEY).toBe("golive.selfview");
    expect(() => writeSelfviewPref(false)).not.toThrow();
    expect(readSelfviewPref()).toBe(true);
  });

  it("share no ar mostra o tile próprio com Ocultar; parado esconde", () => {
    const live = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          snapshot: snapshotFixture({ share: { id: "s1", state: "live" } }),
        }),
      ),
    );
    expect(live).toContain('data-hook="tile-self"');
    expect(live).toContain("Sua transmissão (prévia local)");
    expect(live).toContain("Ocultar");
    expect(live).toContain("PRÉVIA");
    expect(live).not.toContain("Sem transmissões");

    const idle = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(idle).not.toContain('data-hook="tile-self"');
    expect(idle).toContain("Sem transmissões");
  });
});

describe("qualidade (espelha QualityProfile; fio bloqueado)", () => {
  it("presets seguem o backend (HIGH = 10000 kbps)", () => {
    expect(QUALITY_PRESETS.low).toMatchObject({ w: 854, h: 480, bitrate_kbps: 800, fps: 15 });
    expect(QUALITY_PRESETS.medium).toMatchObject({ w: 1280, h: 720, bitrate_kbps: 2000, fps: 30 });
    expect(QUALITY_PRESETS.high).toMatchObject({ w: 1920, h: 1080, bitrate_kbps: 10000, fps: 60 });
  });

  it("dimensão custom: inteira, par, 2–8192", () => {
    expect(validateCustomDim("", "largura")).not.toBeNull();
    expect(validateCustomDim("12.5", "largura")).not.toBeNull();
    expect(validateCustomDim("0", "largura")).not.toBeNull();
    expect(validateCustomDim("1", "altura")).not.toBeNull();
    expect(validateCustomDim("8193", "largura")).not.toBeNull();
    expect(validateCustomDim("641", "largura")).toContain("par");
    expect(validateCustomDim("640", "largura")).toBeNull();
    expect(validateCustomDim("4096", "altura")).toBeNull();
    expect(validateCustomDim("5120", "largura")).toBeNull();
  });

  it("bitrate 100–20000 kbps, fps 1–60", () => {
    expect(validateBitrate("99")).not.toBeNull();
    expect(validateBitrate("100")).toBeNull();
    expect(validateBitrate("20000")).toBeNull();
    expect(validateBitrate("20001")).not.toBeNull();
    expect(validateFps("0")).not.toBeNull();
    expect(validateFps("1")).toBeNull();
    expect(validateFps("60")).toBeNull();
    expect(validateFps("61")).not.toBeNull();
  });

  it("resolve preset + custom válido", () => {
    const preset = resolveDesired({
      resolution: "720p",
      customW: "",
      customH: "",
      quality: "medium",
      customBitrate: "",
      customFps: "",
      srcDims: null,
    });
    expect(preset).toEqual({ profile: { w: 1280, h: 720, bitrate_kbps: 2000, fps: 30 } });

    const custom = resolveDesired({
      resolution: "custom",
      customW: "640",
      customH: "360",
      quality: "custom",
      customBitrate: "1000",
      customFps: "24",
      srcDims: { w: 1920, h: 1080 },
    });
    expect(custom).toEqual({ profile: { w: 640, h: 360, bitrate_kbps: 1000, fps: 24 } });
  });

  it("1:1 usa a fonte par ou o teto 8192 quando a fonte é desconhecida", () => {
    const native = resolveDesired({
      resolution: "1:1",
      customW: "",
      customH: "",
      quality: "high",
      customBitrate: "",
      customFps: "",
      srcDims: { w: 3440, h: 1440 },
    });
    expect(native).toEqual({ profile: { w: 3440, h: 1440, bitrate_kbps: 10000, fps: 60 } });

    const unknown = resolveDesired({
      resolution: "1:1",
      customW: "",
      customH: "",
      quality: "high",
      customBitrate: "",
      customFps: "",
      srcDims: { w: 0, h: 0 },
    });
    expect(unknown).toEqual({ profile: { w: 8192, h: 8192, bitrate_kbps: 10000, fps: 60 } });
    const uwqhd = resolveDesired({
      resolution: "5120x1440",
      customW: "",
      customH: "",
      quality: "high",
      customBitrate: "",
      customFps: "",
      srcDims: { w: 5120, h: 1440 },
    });
    expect(uwqhd).toEqual({ profile: { w: 5120, h: 1440, bitrate_kbps: 10000, fps: 60 } });
  });

  it("janela 0×0 não barra custom acima de 1080p", () => {
    const custom = resolveDesired({
      resolution: "custom",
      customW: "2560",
      customH: "1440",
      quality: "high",
      customBitrate: "",
      customFps: "",
      srcDims: { w: 0, h: 0 },
    });
    expect(custom).toEqual({ profile: { w: 2560, h: 1440, bitrate_kbps: 10000, fps: 60 } });
  });

  it("barra upscale além da fonte conhecida; sem fonte, sem teto extra", () => {
    const over = resolveDesired({
      resolution: "custom",
      customW: "3840",
      customH: "2160",
      quality: "low",
      customBitrate: "",
      customFps: "",
      srcDims: { w: 1920, h: 1080 },
    });
    expect("errors" in over && over.errors.join(" ")).toContain("sem upscale");

    const unknown = resolveDesired({
      resolution: "custom",
      customW: "3840",
      customH: "2160",
      quality: "low",
      customBitrate: "",
      customFps: "",
      srcDims: null,
    });
    expect(unknown).toEqual({ profile: { w: 3840, h: 2160, bitrate_kbps: 800, fps: 15 } });
  });

  it("custom incompleto lista todos os erros", () => {
    const result = resolveDesired({
      resolution: "custom",
      customW: "641",
      customH: "",
      quality: "custom",
      customBitrate: "50",
      customFps: "0",
      srcDims: null,
    });
    expect("errors" in result && result.errors.length).toBeGreaterThanOrEqual(4);
  });

  it("intenção de share: válido vira perfil, inválido vira erro (nunca default silencioso)", () => {
    const valid = shareIntentFromResolved({
      profile: { w: 5120, h: 1440, bitrate_kbps: 10000, fps: 60 },
    });
    expect(valid).toEqual({ profile: { w: 5120, h: 1440, bitrate_kbps: 10000, fps: 60 } });
    const invalid = shareIntentFromResolved({ errors: ["sem upscale além da fonte (5120×1440)."] });
    expect("error" in invalid && invalid.error).toContain("sem upscale");
  });

  it("portão de refresh: frame-event só pede snapshot 1×/s", () => {
    expect(frameRefreshDue(0, 999)).toBe(false);
    expect(frameRefreshDue(0, 1000)).toBe(true);
    expect(frameRefreshDue(1000, 1500)).toBe(false);
    expect(frameRefreshDue(1000, 2000)).toBe(true);
    expect(frameRefreshDue(1000, 2001)).toBe(true);
  });
});

describe("formatadores de contadores", () => {
  it("bps em pt-BR, delay honesto", () => {
    expect(formatBps(800)).toBe("800 bps");
    expect(formatBps(1800)).toContain("kbps");
    expect(formatBps(1_800_000)).toContain("Mbps");
    expect(formatBps(-1)).toBe("—");
    expect(formatFps(29.7)).toBe("29.7 fps");
    expect(formatDelayMs(null)).toBe("—");
    expect(formatDelayMs(42)).toBe("42 ms");
  });
});

describe("QualityPanel (integrado na sala)", () => {
  it("staging no popup de fonte libera o desejo sem share vivo", () => {
    const html = renderToStaticMarkup(
      createElement(QualityPanel, qualityFixture({ shareLive: false, staging: true })),
    );
    expect(html).not.toContain(QUALITY_DISABLED_REASON);
    expect(html).toContain("5120×1440");
    expect(html).toContain("Este perfil entra junto com Compartilhar");
  });

  it("share inativo: tudo desabilitado com motivo", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ quality: qualityFixture({ shareLive: false }) })),
    );
    expect(html).toContain(QUALITY_DISABLED_REASON);
    expect(html).toContain("disabled");
  });

  it("share no ar: desejo + botão Aplicar + efetivo do backend", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(html).toContain("Desejado: 1280×720 @ 2000 kbps · 30 fps");
    expect(html).toContain("Aplicar qualidade");
    // Sem leitura ainda: honesto, sem número inventado.
    expect(html).toContain("Efetivo no backend: ainda sem leitura");
  });

  it("efetivo autoritativo mostra perfil + geração", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          quality: qualityFixture({
            effective: { profile: { w: 640, h: 360, bitrate_kbps: 1000, fps: 24 }, generation: 3 },
          }),
        }),
      ),
    );
    expect(html).toContain("Efetivo no backend: 640×360 @ 1000 kbps · 24 fps · geração 3");
    // O staged continua visível, mas não se passa por efetivo.
    expect(html).toContain("Desejado: 1280×720 @ 2000 kbps · 30 fps");
  });

  it("selo do codificador: hardware, software com motivo, sem leitura", () => {
    const hw = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ quality: qualityFixture({ backend: "videotoolbox" }) })),
    );
    expect(hw).toContain("Codificador: VideoToolbox (hardware)");
    const nvenc = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ quality: qualityFixture({ backend: "nvenc" }) })),
    );
    expect(nvenc).toContain("Codificador: NVENC (hardware)");
    expect(nvenc).toContain('data-testid="diag-backend">Codificador: NVENC (hardware)');
    const sw = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          quality: qualityFixture({
            backend: "openh264",
            backendNote: "probe de hardware falhou — ver log de sessão",
          }),
        }),
      ),
    );
    expect(sw).toContain("Codificador: OpenH264 (software)");
    expect(sw).toContain("probe de hardware falhou");
    const plain = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({ quality: qualityFixture({ backend: "openh264", backendNote: null }) }),
      ),
    );
    expect(plain).toContain("Codificador: OpenH264 (software)");
    expect(plain).toContain(
      'data-testid="encode-backend">Codificador: OpenH264 (software)<',
    );
    const none = renderToStaticMarkup(createElement(RoomScreen, roomProps()));
    expect(none).toContain("Codificador: ainda sem leitura");
  });

  it("selo nunca inventa rótulo para backend desconhecido", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ quality: qualityFixture({ backend: "fooenc" }) })),
    );
    expect(html).toContain("Codificador: ainda sem leitura");
    expect(html).not.toContain("fooenc");
  });

  it("aplicando mostra progresso; erro do comando sai verbatim", () => {
    const busy = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ quality: qualityFixture({ applying: true }) })),
    );
    expect(busy).toContain("Aplicando…");

    const failed = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({ quality: qualityFixture({ applyError: "qualidade: share parado" }) }),
      ),
    );
    expect(failed).toContain("qualidade: share parado");
  });

  it("perfil inválido desabilita o Aplicar", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          quality: qualityFixture({ resolution: "custom", customW: "641", customH: "" }),
        }),
      ),
    );
    // Botão desabilitado com motivo; fieldset segue editável para corrigir.
    expect(html).toContain("Aplicar qualidade");
    expect(html).toContain("disabled");
    expect(html).toContain("Corrija os erros do perfil custom.");
    expect(html).not.toContain("<fieldset disabled");
  });

  it("custom mostra os campos editáveis", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          quality: qualityFixture({
            resolution: "custom",
            customW: "640",
            customH: "360",
            quality: "custom",
            customBitrate: "1000",
            customFps: "24",
          }),
        }),
      ),
    );
    expect(html).toContain("custom-w");
    expect(html).toContain("custom-bitrate");
    expect(html).toContain("Desejado: 640×360 @ 1000 kbps · 24 fps");
  });

  it("custom inválido esconde o perfil sem lista de erros (só o Aplicar gata)", () => {
    const html = renderToStaticMarkup(
      createElement(
        RoomScreen,
        roomProps({
          quality: qualityFixture({ resolution: "custom", customW: "641", customH: "" }),
        }),
      ),
    );
    expect(html).toContain("par");
    expect(html).not.toContain("Desejado:");
    expect(html).not.toContain('class="errors"');
  });
});

describe("ViewerLinksPanel (por link, só backend)", () => {
  it("renderiza FPS, delay honesto, codec, bitrate, resolução e contadores", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ linkStats: [linkFixture()], watching: ["m-2"] })),
    );
    expect(html).toContain("Bia");
    expect(html).toContain("H.264 Constrained Baseline");
    expect(html).toContain("29.7 fps");
    expect(html).toContain("1280×720");
    expect(html).toContain("120");
    expect(html).toContain("118");
    // Delay None do backend: "—" + nota honesta como hint.
    expect(html).toContain("—");
    expect(html).toContain("RTT do par ICE nao exposto pelo core");
    expect(html).toContain(WINDOW_HINT);
  });

  it("delay presente mostra ms", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ linkStats: [linkFixture({ delay_estimate_ms: 42 })] })),
    );
    expect(html).toContain("42 ms");
  });

  it("sem amostra: diagnóstico, nunca zeros inventados", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ linkStats: null })));
    expect(html).toContain("Sem amostra de contadores ainda");
  });

  it("watch pedido sem link: aguardando backend", () => {
    const html = renderToStaticMarkup(
      createElement(RoomScreen, roomProps({ linkStats: [], watching: ["m-2"] })),
    );
    expect(html).toContain("aguardando o link do backend");
  });

  it("nada assistido: como assistir", () => {
    const html = renderToStaticMarkup(createElement(RoomScreen, roomProps({ linkStats: [] })));
    expect(html).toContain("Nenhum link assistido");
  });
});
