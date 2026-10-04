import { StreamPlayer } from "./StreamPlayer";
/**
 * Sala mínima: intenções explícitas + snapshots do dono + 2 eventos Tauri.
 *
 * - Comandos só via `api.ts`, chamados de handlers (clique) ou de callbacks
 *   de evento. NENHUM setInterval/setTimeout neste módulo.
 * - Snapshot via `get_snapshot` sob demanda (botão Atualizar, respostas de
 *   intent, chegada de evento). Roster rico via evento "roster".
 * - Erros do backend já vêm redigidos (sem senha/token/SDP); a UI nunca
 *   exibe de volta o conteúdo dos campos de senha.
 */

import { useEffect, useRef, useState } from "react";
import { version as APP_VERSION } from "../package.json";
import { checkForUpdate, openUpdateUrl, type UpdateInfo } from "./update";
import {
  createRoom,
  getE2ePlan,
  getMediaCounters,
  getRoster,
  getSnapshot,
  joinRoom,
  leaveRoom,
  onMediaEvent,
  onSignalEvent,
  previewSource,
  setQuality as setQualityCommand,
  setServer,
  startShare,
  stopShare,
  unwatchMember,
  watchMember,
  listAudioApps,
  listSources,
  setAudioExclusions,
  sourceCapabilities,
  type AudioApp,
  type CapabilitySet,
  type E2ePlan,
  type EffectiveQuality,
  type LinkStats,
  type MediaEvent,
  type OwnerSnapshot,
  type RoomMember,
  type SignalEvent,
  type SourceInfo,
  type ViewerStats,
} from "./api";
import { runE2ePlan, type E2eReport } from "./e2e";
import {
  MOCK_SELF_ID,
  isTauriMissing,
  mockCaps,
  mockCounters,
  mockEffective,
  mockRoster,
  mockSnapshot,
  mockSources,
  randomMockCode,
} from "./mock";
import {
  HomeScreen,
  RoomScreen,
  UpdateModal,
  NICKNAME_STORAGE_KEY,
  SERVER_STORAGE_KEY,
  defaultExcludedAppIds,
  frameRefreshDue,
  readStoredSetting,
  resolveDesired,
  shareIntentFromResolved,
  watchingStillLive,
  writeStoredSetting,
  validateCode,
  validateNickname,
  validatePassword,
  validateSource,
  type QualitySel,
  type ResolutionSel,
} from "./views";

export const DEFAULT_SERVER = "https://together.jouymaker.com";

const signalSummary = (event: SignalEvent): string => {
  switch (event.kind) {
    case "roster":
      return `roster (${event.entries.length} membro(s))`;
    case "watch":
      return `watch de ${event.from}`;
    case "unwatch":
      return `unwatch de ${event.from}`;
    case "signal":
      return `sinal ${event.type} de ${event.from}`;
    default:
      return event.kind;
  }
};

const mediaSummary = (event: MediaEvent): string => {
  switch (event.kind) {
    case "frame":
      return `frame (não-preto: ${event.non_black ? "sim" : "não"})`;
    case "stats":
      return `stats (${event.frames} frames)`;
    case "quality":
      return `qualidade (geração ${event.generation})`;
    default:
      return event.kind;
  }
};

export default function App() {
  const [screen, setScreen] = useState<"home" | "room">("home");
  const [tab, setTab] = useState<"create" | "join">("create");
  // Servidor + apelido persistem no localStorage (volta a home preenchida);
  // vazios/ausentes caem nos fallbacks de sempre (DEFAULT_SERVER/"Convidado").
  const [server, setServerBase] = useState(() => readStoredSetting(SERVER_STORAGE_KEY) ?? DEFAULT_SERVER);
  // Apelido interno (sem input visível na home fiel ao goDrinking2).
  const [nickname, setNickname] = useState(() => readStoredSetting(NICKNAME_STORAGE_KEY) ?? "Convidado");
  const [password, setPassword] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [roomCode, setRoomCode] = useState<string | null>(null);
  const [selfId, setSelfId] = useState<string | null>(null);
  const [snapshot, setSnapshot] = useState<OwnerSnapshot | null>(null);
  const [roster, setRoster] = useState<RoomMember[]>([]);
  const [watching, setWatching] = useState<string[]>([]);
  const [source, setSource] = useState("synthetic");
  // Fontes de captura: capacidades (puro, sem SO) ao entrar na sala;
  // a lista real só em gesto explícito (pode pedir permissão ao SO).
  const [sources, setSources] = useState<SourceInfo[]>([]);
  const [sourcesError, setSourcesError] = useState<string | null>(null);
  // Permissão de captura negada (marcador "Gravação de Tela" do backend):
  // a UI renderiza o bloco honesto com o caminho manual. Sem retry.
  const [sourcesDenied, setSourcesDenied] = useState(false);
  const selectedSourceDims = (): { w: number; h: number } | null => {
    const selected = sources.find((item) => `${item.kind}:${item.id}` === source.trim());
    if (!selected || selected.w < 2 || selected.h < 2) return null;
    return { w: selected.w, h: selected.h };
  };
  // Thumbs PNG (data URL) por "kind:id": cache lazy do modal Compartilhar
  // (busca sob demanda via handlePreviewsVisible; mock nunca busca).
  const [previews, setPreviews] = useState<Record<string, string>>({});
  // Chaves já pedidas (ok ou null): evita refetch e rajadas; limpo a cada
  // listagem/saída para acompanhar a lista fresca.
  const previewsSeen = useRef<Set<string>>(new Set());
  const previewQueue = useRef<SourceInfo[]>([]);
  const previewInFlight = useRef(0);
  const [caps, setCaps] = useState<CapabilitySet | null>(null);
  const [audioApps, setAudioApps] = useState<AudioApp[]>([]);
  const [audioExcluded, setAudioExcluded] = useState<string[]>([]);
  // Ids que o host destildou à mão: os defaults (Discord/próprio app) nunca
  // re-adicionam esses ids ao recarregar a lista. Limpo ao parar o share.
  const audioUserKept = useRef<Set<string>>(new Set());
  const [lastSignal, setLastSignal] = useState<string | null>(null);
  const [lastMedia, setLastMedia] = useState<string | null>(null);
  const [stats, setStats] = useState<ViewerStats | null>(null);
  // Contadores por link: mesma fonte do refresh + evento stats (que já pode
  // trazer `links` no lado viewer). Nenhum polling novo.
  const [linkStats, setLinkStats] = useState<LinkStats[] | null>(null);
  // Perfil de qualidade desejado (staged na UI; Aplicar chama `set_quality`
  // e o efetivo autoritativo volta no snapshot/evento).
  const [resolution, setResolution] = useState<ResolutionSel>("720p");
  const [customW, setCustomW] = useState("");
  const [customH, setCustomH] = useState("");
  const [quality, setQuality] = useState<QualitySel>("medium");
  const [customBitrate, setCustomBitrate] = useState("");
  const [customFps, setCustomFps] = useState("");
  // Efetivo autoritativo (snapshot + evento quality); aplicação em voo e o
  // último erro do comando (verbatim, já redatado no backend).
  const [effective, setEffective] = useState<EffectiveQuality | null>(null);
  // Selo do codificador (snapshot + evento stats; sem polling novo).
  const [backend, setBackend] = useState<string | null>(null);
  const [backendNote, setBackendNote] = useState<string | null>(null);
  const [applying, setApplying] = useState(false);
  const [applyError, setApplyError] = useState<string | null>(null);
  // Último refresh completo disparado por frame-event (ver
  // frameRefreshDue): frame-events chegam a 30–60/s por stream e um
  // refresh custa 4 IPCs + enumeração de áudio — sem portão a UI congela.
  const lastFrameRefresh = useRef(0);

  // Modo autodirigido test-only: só ativa com `--e2e-plan` (get_e2e_plan
  // devolve null no app normal e nada aqui executa). Guarda contra
  // mount duplo; o driver é headless e reporta via status + título.
  // Modo mock automático no navegador puro (sem Tauri): `isMock` congela na
  // montagem via `isTauriMissing()` (ausência de `window.__TAURI__` /
  // `window.__TAURI_INTERNALS__`). Quando true, nenhum `invoke`/`listen` é
  // chamado — tudo é estado local via `mock.ts`.
  const [isMock] = useState(() => isTauriMissing());
  // Share mock (fonte de verdade do snapshot mock; o backend real usa o
  // snapshot para isso, aqui o toggle local alimenta `mockSnapshot`).
  const [mockSharing, setMockSharing] = useState(true);
  const [e2ePlan, setE2ePlan] = useState<E2ePlan | null>(null);
  const [e2eReport, setE2eReport] = useState<E2eReport | null>(null);
  useEffect(() => {
    if (isMock) return; // mock: sem Tauri, sem e2e, sem invoke
    let live = true;
    let started = false;
    getE2ePlan()
      .then((plan) => {
        if (!live || !plan || started) return;
        started = true;
        setE2ePlan(plan);
        void runE2ePlan(plan, (report) => {
          if (live) {
            setE2eReport(report);
            document.title = `goDrinking e2e ${report.role} ${report.phase}`;
          }
        }).catch((failure: unknown) => {
          if (live) {
            setE2eReport({
              role: plan.role,
              phase: "error",
              connected: false,
              frames: 0,
              keyframes: 0,
              keyframesSeen: false,
              presented: 0,
              qualityApplied: false,
              detail: typeof failure === "string" ? failure : failure instanceof Error ? failure.message : "e2e failed",
            });
          }
        });
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [isMock]);

  const messageOf = (failure: unknown, fallback: string): string => {
    if (failure instanceof Error) return failure.message;
    if (typeof failure === "string") return failure;
    return fallback;
  };

  // Home persistente: Servidor + Seu Nick voltam preenchidos (fallbacks de
  // create/join — `trim() || DEFAULT` — continuam valendo para vazio).
  useEffect(() => {
    writeStoredSetting(SERVER_STORAGE_KEY, server);
  }, [server]);
  useEffect(() => {
    writeStoredSetting(NICKNAME_STORAGE_KEY, nickname);
  }, [nickname]);

  /**
   * Mescla os defaults (Discord + próprio app/helper) no `audioExcluded` ao
   * carregar a lista, avisando o backend para o tap excluir de verdade.
   * Ids destildados à mão (audioUserKept) nunca voltam sozinhos.
   */
  const seedDefaultAudioExclusions = (apps: AudioApp[]): void => {
    const matched = defaultExcludedAppIds(apps).filter((id) => !audioUserKept.current.has(id));
    if (matched.length === 0) return;
    setAudioExcluded((current) => {
      const fresh = matched.filter((id) => !current.includes(id));
      if (fresh.length === 0) return current;
      const next = [...current, ...fresh];
      if (!isMock) void setAudioExclusions(next).catch(() => undefined);
      return next;
    });
  };

  /** Lê snapshot + contadores agora (botão, pós-intent, pós-evento). */
  const refresh = async (): Promise<void> => {
    if (isMock) {
      // mock: sem invoke — deriva tudo do estado local (share/watch/efetivo).
      const eff = effective ?? mockEffective();
      setSnapshot(mockSnapshot(mockSharing, watching));
      setRoster(mockRoster(nickname.trim() || "Convidado", mockSharing));
      const counters = mockCounters(watching, eff);
      setLinkStats(counters.links);
      setEffective(eff);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
      setStats((current) =>
        current ?? {
          frames: counters.frames,
          keyframes: counters.keyframes,
          ice: counters.connected,
          presented: counters.presented,
        },
      );
      return;
    }
    let snap: OwnerSnapshot | null = null;
    try {
      snap = await getSnapshot();
      setSnapshot(snap);
    } catch (failure) {
      setError(messageOf(failure, "Não foi ler o estado da sala."));
      return;
    }
    try {
      const entries = await getRoster();
      setRoster(entries);
      setSelfId((current) => current ?? entries.find((entry) => entry.master)?.id ?? null);
      setWatching((current) => {
        const next = watchingStillLive(current, entries);
        for (const id of current.filter((id) => !next.includes(id))) {
          void unwatchMember(id).catch(() => undefined);
        }
        return next;
      });
    } catch {
      setRoster((current) => current);
    }
    try {
      const counters = await getMediaCounters();
      setLinkStats(counters.links);
      setEffective(counters.effective ?? null);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
    } catch {
      // Contadores são fallback observacional: sem eles, o painel de links
      // mostra o diagnóstico honesto em vez de número inventado.
      setLinkStats((current) => current);
    }
    try {
      if (snap?.share.state === "live" || snap?.share.state === "starting") {
        const apps = await listAudioApps();
        setAudioApps(apps);
        seedDefaultAudioExclusions(apps);
      } else {
        setAudioApps([]);
        setAudioExcluded([]);
        audioUserKept.current.clear();
      }
    } catch {
      setAudioApps([]);
    }
  };

  // Escuta única dos 2 eventos, só dentro da sala, com cleanup.
  // Cada evento atualiza o que é dele (roster/stats) e pede um snapshot
  // fresco — dirigido a evento, nunca a timer.
  useEffect(() => {
    if (screen !== "room") return;
    if (isMock) return; // mock: onSignal/onMedia viram no-op (dados já locais)
    let cancelled = false;
    const unlistens: Array<() => void> = [];
    void onSignalEvent((event) => {
      if (cancelled) return;
      if (event.kind === "roster") {
        setRoster(event.entries);
        setSelfId((current) => current ?? event.entries.find((entry) => entry.master)?.id ?? null);
        setWatching((current) => {
          const next = watchingStillLive(current, event.entries);
          const dropped = current.filter((id) => !next.includes(id));
          for (const id of dropped) {
            void unwatchMember(id).catch(() => undefined);
          }
          return next;
        });
      } else if (event.kind === "kicked") {
        setError("Você foi removido da sala.");
      }
      setLastSignal(signalSummary(event));
      void refresh();
    }).then((off) => {
      if (cancelled) off();
      else unlistens.push(off);
    });
    void onMediaEvent((event) => {
      if (cancelled) return;
      if (event.kind === "stats") {
        setStats({ frames: event.frames, keyframes: event.keyframes, ice: event.ice, presented: event.presented ?? 0 });
        if (event.links) setLinkStats(event.links);
        if (event.backend !== undefined) setBackend(event.backend);
        if (event.backend_note !== undefined) setBackendNote(event.backend_note);
      } else if (event.kind === "quality") {
        setEffective({ profile: event.profile, generation: event.generation });
      } else if (event.kind === "ice-failed") {
        setError("Falha ao conectar. Tente ver de novo.");
        setWatching([]);
      }
      setLastMedia(mediaSummary(event));
      if (event.kind === "frame") {
        // Frame-events chegam por frame decodificado: refresh completo aqui
        // congela a UI (ver frameRefreshDue) — texto de liveness atualiza
        // sempre, snapshot só no portão de 1/s (stats cobrem o resto).
        const now = Date.now();
        if (frameRefreshDue(lastFrameRefresh.current, now)) {
          lastFrameRefresh.current = now;
          void refresh();
        }
      } else {
        void refresh();
      }
    }).then((off) => {
      if (cancelled) off();
      else unlistens.push(off);
    });
    return () => {
      cancelled = true;
      for (const off of unlistens) off();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [screen]);

  useEffect(() => {
    if (isMock || screen !== "room") return;
    const live = snapshot?.share.state === "live" || snapshot?.share.state === "starting";
    if (!live) {
      setAudioApps([]);
      setAudioExcluded([]);
      audioUserKept.current.clear();
      return;
    }
    let cancelled = false;
    const pull = () => {
      listAudioApps().then(
        (apps) => {
          if (cancelled) return;
          setAudioApps(apps);
          seedDefaultAudioExclusions(apps);
        },
        () => {
          if (!cancelled) setAudioApps([]);
        },
      );
    };
    pull();
    const timer = window.setInterval(pull, 1000);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [isMock, screen, snapshot?.share.state]);

  const runIntent = async (work: () => Promise<void>): Promise<void> => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await work();
      await refresh();
    } catch (failure) {
      setError(messageOf(failure, "A operação falhou."));
    } finally {
      setBusy(false);
    }
  };

  const handleCreate = (): void => {
    if (isMock) {
      // mock: pula TODAS as validações do caminho real (apelido/senha) —
      // o lobby fiel não tem campo de apelido e a senha é opcional aqui.
      // Entra sempre com sucesso.
      const effectiveNickname = nickname.trim() || "Convidado";
      const created = randomMockCode();
      const eff = mockEffective();
      const counters = mockCounters([], eff);
      setRoomCode(created);
      setNickname(effectiveNickname);
      setSelfId(MOCK_SELF_ID);
      setMockSharing(true);
      setSnapshot(mockSnapshot(true, []));
      setRoster(mockRoster(effectiveNickname, true));
      setWatching([]);
      setEffective(eff);
      setLinkStats(counters.links);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
      setStats({
        frames: counters.frames,
        keyframes: counters.keyframes,
        ice: counters.connected,
        presented: counters.presented,
      });
      setCaps(mockCaps());
      setSources([]);
      setSourcesError(null);
      setLastSignal("roster (4 membro(s))");
      setLastMedia(null);
      setError(null);
      setScreen("room");
      return;
    }
    // Caminho Tauri real: validações mantidas como estão.
    const effectiveNickname = nickname.trim() || "Convidado";
    const nameError = validateNickname(effectiveNickname);
    if (nameError) {
      setError(nameError);
      return;
    }
    const passError = validatePassword(password);
    if (passError) {
      setError(passError);
      return;
    }
    const base = server.trim() || DEFAULT_SERVER;
    void runIntent(async () => {
      await setServer(base);
      const created = await createRoom(effectiveNickname, password);
      setRoomCode(created);
      setNickname(effectiveNickname);
      setSelfId(null); // create devolve código; nosso id chega no roster
      setScreen("room");
      // Capacidades são fatos de compilação (sem SO): seguro buscar ao entrar.
      // A lista real de fontes só em gesto explícito (pode pedir permissão).
      sourceCapabilities().then(setCaps, () => undefined);
    });
  };

  const handleJoin = (): void => {
    if (isMock) {
      // mock: pula TODAS as validações do caminho real (apelido/senha/código).
      // Usa o digitado ou gera um; entra sempre com sucesso.
      const effectiveNickname = nickname.trim() || "Convidado";
      const joined = code.trim().toUpperCase() || randomMockCode();
      const eff = mockEffective();
      const counters = mockCounters([], eff);
      setRoomCode(joined);
      setNickname(effectiveNickname);
      setSelfId(MOCK_SELF_ID);
      setMockSharing(true);
      setSnapshot(mockSnapshot(true, []));
      setRoster(mockRoster(effectiveNickname, true));
      setWatching([]);
      setEffective(eff);
      setLinkStats(counters.links);
      setBackend(counters.backend ?? null);
      setBackendNote(counters.backend_note ?? null);
      setStats({
        frames: counters.frames,
        keyframes: counters.keyframes,
        ice: counters.connected,
        presented: counters.presented,
      });
      setCaps(mockCaps());
      setSources([]);
      setSourcesError(null);
      setLastSignal("roster (4 membro(s))");
      setLastMedia(null);
      setError(null);
      setScreen("room");
      return;
    }
    // Caminho Tauri real: validações mantidas como estão.
    const effectiveNickname = nickname.trim() || "Convidado";
    const nameError = validateNickname(effectiveNickname);
    if (nameError) {
      setError(nameError);
      return;
    }
    const passError = validatePassword(password);
    if (passError) {
      setError(passError);
      return;
    }
    const codeError = validateCode(code);
    if (codeError) {
      setError(codeError);
      return;
    }
    const base = server.trim() || DEFAULT_SERVER;
    void runIntent(async () => {
      await setServer(base);
      const memberId = await joinRoom(code.trim().toUpperCase(), effectiveNickname, password);
      setRoomCode(code.trim().toUpperCase());
      setNickname(effectiveNickname);
      setSelfId(memberId);
      setScreen("room");
      sourceCapabilities().then(setCaps, () => undefined);
    });
  };

  const handleLeave = (): void => {
    if (isMock) {
      // mock: só limpa o estado local.
      setScreen("home");
      setSnapshot(null);
      setRoster([]);
      setWatching([]);
      setStats(null);
      setLinkStats(null);
      setEffective(null);
      setApplying(false);
      setApplyError(null);
      setSources([]);
      setSourcesError(null);
      setCaps(null);
      setLastSignal(null);
      setLastMedia(null);
      setRoomCode(null);
      setSelfId(null);
      setPassword("");
      setMockSharing(true);
      setPreviews({});
      previewsSeen.current.clear();
      setSourcesDenied(false);
      return;
    }
    void runIntent(async () => {
      await leaveRoom().catch(() => undefined);
      setScreen("home");
      setSnapshot(null);
      setRoster([]);
      setWatching([]);
      setStats(null);
      setLinkStats(null);
      setEffective(null);
      setApplying(false);
      setApplyError(null);
      setSources([]);
      setSourcesError(null);
      setCaps(null);
      setLastSignal(null);
      setLastMedia(null);
      setRoomCode(null);
      setSelfId(null);
      setPassword("");
      setPreviews({});
      previewsSeen.current.clear();
      setSourcesDenied(false);
    });
  };

  const handleListSources = (): void => {
    if (isMock) {
      // mock: lista local, sem pedir permissão ao SO.
      setSourcesError(null);
      setSources(mockSources());
      return;
    }
    setSourcesError(null);
    listSources().then(
      (listed) => {
        setSources(listed);
        // Lista fresca → thumbs frescos (limpa o cache lazy).
        setPreviews({});
        previewsSeen.current.clear();
        if (listed.length === 0) {
          setSourcesError("Nenhuma fonte visível — provavelmente falta permissão de Gravação de Tela.");
          setSourcesDenied(true);
        } else {
          setSourcesDenied(false);
        }
      },
      (failure: unknown) => {
        setSources([]);
        const message = messageOf(failure, "Não foi listar as fontes.");
        setSourcesError(message);
        // Marcador do HINT do backend ("Gravação de Tela"): bloco honesto na
        // UI. Sem opener nativo no shell → caminho manual, sem botão.
        setSourcesDenied(message.includes("Gravação de Tela"));
      },
    );
  };

  /**
   * Busca lazy dos thumbs da aba visível do modal (views avisa ao abrir /
   * trocar de aba, com debounce). Cache por kind:id + dedupe de voo:
   * null/falha marcam como visto (sem retry em rajada) e nunca quebram as
   * outras fontes. Mock nunca busca (gradiente local).
   */
  const handlePreviewsVisible = (items: SourceInfo[]): void => {
    if (isMock || items.length === 0) return;
    const pump = (): void => {
      while (previewInFlight.current < 3) {
        const item = previewQueue.current.shift();
        if (!item) return;
        const key = `${item.kind}:${item.id}`;
        previewInFlight.current += 1;
        previewSource(item.kind, item.id).then(
          (preview) => {
            if (preview.data_url) {
              const dataUrl: string = preview.data_url;
              setPreviews((current) => ({ ...current, [key]: dataUrl }));
            }
          },
          () => undefined,
        ).finally(() => {
          previewInFlight.current = Math.max(0, previewInFlight.current - 1);
          pump();
        });
      }
    };
    for (const item of items) {
      const key = `${item.kind}:${item.id}`;
      if (previews[key] !== undefined || previewsSeen.current.has(key)) continue;
      previewsSeen.current.add(key);
      previewQueue.current.push(item);
    }
    pump();
  };

  const runShare = (descriptor: string): void => {
    const sourceError = validateSource(descriptor);
    if (sourceError) {
      setError(sourceError);
      return;
    }
    if (isMock) {
      // mock: só atualiza o estado local (snapshot/roster/efetivo).
      const eff = effective ?? mockEffective();
      setMockSharing(true);
      setSnapshot(mockSnapshot(true, watching));
      setRoster(mockRoster(nickname.trim() || "Convidado", true));
      setEffective(eff);
      setLastMedia("frame (não-preto: sim)");
      setError(null);
      return;
    }
    void runIntent(async () => {
      const resolved = resolveDesired({
        resolution,
        customW,
        customH,
        quality,
        customBitrate,
        customFps,
        srcDims: selectedSourceDims(),
      });
      const intent = shareIntentFromResolved(resolved);
      if ("error" in intent) {
        throw new Error(intent.error);
      }
      await startShare(descriptor.trim(), intent.profile);
      setEffective({ profile: intent.profile, generation: 0 });
    });
  };

  const handleShare = (): void => {
    runShare(source);
  };

  /**
   * Tela + webcam num feed só (PiP). O descritor `combo:<tela>+camera:<id>`
   * é validado aqui; o backend compõe e publica como um share comum
   * (o viewer vê sem protocolo novo).
   */
  const handleShareCombo = (screen: string, cameraId: string): void => {
    const screenTrimmed = screen.trim();
    const cameraTrimmed = cameraId.trim();
    const screenTag = screenTrimmed.startsWith("display:")
      ? screenTrimmed
      : screenTrimmed.startsWith("window:")
        ? screenTrimmed
        : null;
    if (!screenTag || !cameraTrimmed) {
      setError("Combo: escolha a tela e a webcam.");
      return;
    }
    runShare(`combo:${screenTag}+camera:${cameraTrimmed}`);
  };

  const handleStopShare = (): void => {
    if (isMock) {
      setMockSharing(false);
      setSnapshot(mockSnapshot(false, watching));
      setRoster(mockRoster(nickname.trim() || "Convidado", false));
      setLastMedia("frame (não-preto: não)");
      setAudioApps([]);
      setAudioExcluded([]);
      audioUserKept.current.clear();
      return;
    }
    void runIntent(async () => {
      await stopShare();
      setAudioApps([]);
      setAudioExcluded([]);
      audioUserKept.current.clear();
    });
  };

  const handleToggleAudioExclude = (id: string): void => {
    const excluding = !audioExcluded.includes(id);
    const next = excluding
      ? [...audioExcluded, id]
      : audioExcluded.filter((item) => item !== id);
    // Destildar à mão marca o id: os defaults não re-adicionam; tildar à mão
    // desmarca (o default volta a valer na próxima lista fresca).
    if (excluding) audioUserKept.current.delete(id);
    else audioUserKept.current.add(id);
    setAudioExcluded(next);
    if (isMock) return;
    void setAudioExclusions(next).catch(() => undefined);
  };

  /**
   * Aplica o perfil desejado ao share vivo. Progresso e erro próprios do
   * painel (não usa o busy global): o comando responde o efetivo pré-bump
   * e a geração autoritativa chega pelo evento `quality`.
   */
  const handleApplyQuality = (): void => {
    if (applying || busy) return;
    const resolved = resolveDesired({
      resolution,
      customW,
      customH,
      quality,
      customBitrate,
      customFps,
      srcDims: selectedSourceDims(),
    });
    if (!("profile" in resolved)) {
      setApplyError(resolved.errors.join(" "));
      return;
    }
    if (isMock) {
      // mock: aplica localmente com bump de geração, sem comando.
      setEffective({
        profile: resolved.profile,
        generation: (effective?.generation ?? 0) + 1,
      });
      setApplying(false);
      setApplyError(null);
      setLastMedia("qualidade (geração mock)");
      return;
    }
    setApplying(true);
    setApplyError(null);
    const preset = quality === "custom" ? undefined : quality;
    setQualityCommand(resolved.profile, preset).then(
      (result) => {
        setEffective(result);
        setApplying(false);
        void refresh();
      },
      (failure: unknown) => {
        setApplyError(messageOf(failure, "Não foi aplicar a qualidade."));
        setApplying(false);
      },
    );
  };

  const handleWatch = (id: string): void => {
    if (isMock) {
      // mock: só atualiza o estado local (links/contadores derivam daqui).
      const next = watching.includes(id) ? watching : [...watching, id];
      const eff = effective ?? mockEffective();
      setWatching(next);
      setSnapshot(mockSnapshot(mockSharing, next));
      setLinkStats(mockCounters(next, eff).links);
      setLastSignal(`watch de ${id}`);
      return;
    }
    void runIntent(async () => {
      await watchMember(id);
      setWatching((current) => (current.includes(id) ? current : [...current, id]));
    });
  };

  const handleUnwatch = (id: string): void => {
    if (isMock) {
      const next = watching.filter((item) => item !== id);
      const eff = effective ?? mockEffective();
      setWatching(next);
      setSnapshot(mockSnapshot(mockSharing, next));
      setLinkStats(mockCounters(next, eff).links);
      setLastSignal(`unwatch de ${id}`);
      return;
    }
    void runIntent(async () => {
      await unwatchMember(id);
      setWatching((current) => current.filter((item) => item !== id));
    });
  };

  // Verificação de atualização: UMA vez no mount (sem polling) + botão
  // manual na home. Falha silenciosa (repo privado/offline = sem popup).
  // Guarda contra mount duplo (StrictMode); exibe uma vez por abertura.
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [updateOpen, setUpdateOpen] = useState(false);
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const updateChecked = useRef(false);
  useEffect(() => {
    if (updateChecked.current) return;
    updateChecked.current = true;
    let live = true;
    checkForUpdate(APP_VERSION).then((info) => {
      if (live && info) {
        setUpdate(info);
        setUpdateOpen(true);
      }
    });
    return () => {
      live = false;
    };
  }, []);

  const handleCheckUpdate = (): void => {
    if (checkingUpdate) return;
    setCheckingUpdate(true);
    checkForUpdate(APP_VERSION).then(
      (info) => {
        setCheckingUpdate(false);
        if (info) {
          setUpdate(info);
          setUpdateOpen(true);
        }
      },
      () => {
        setCheckingUpdate(false);
      },
    );
  };

  const updateModal = (
    <UpdateModal
      current={APP_VERSION}
      info={update}
      open={updateOpen}
      onClose={() => setUpdateOpen(false)}
      onOpenUrl={(url) => {
        void openUpdateUrl(url);
      }}
    />
  );

  if (e2ePlan) {
    const phase = e2eReport?.phase ?? "boot";
    const summary =
      `e2e ${e2ePlan.role} ${phase} ` +
      `connected=${e2eReport?.connected ? "yes" : "no"} ` +
      `frames=${e2eReport?.frames ?? 0} ` +
      `keyframes=${e2eReport?.keyframes ?? 0} ` +
      `presented=${e2eReport?.presented ?? 0}` +
      (e2eReport?.code ? ` code=${e2eReport.code}` : "") +
      (e2eReport?.detail ? ` ${e2eReport.detail}` : "");
    return (
      <main data-testid="e2e-status">
        <h1>goDrinking e2e {e2ePlan.role}</h1>
        <p>{summary}</p>
        {e2eReport?.watchedMember ? <StreamPlayer member={e2eReport.watchedMember} nickname="host-e2e" /> : null}
      </main>
    );
  }

  if (screen === "home") {
    return (
      <>
        <HomeScreen
          server={server}
          onServer={setServerBase}
          defaultServer={DEFAULT_SERVER}
          tab={tab}
          onTab={setTab}
          nickname={nickname}
          onNickname={setNickname}
          password={password}
          onPassword={setPassword}
          code={code}
          onCode={setCode}
          busy={busy}
          error={error}
          onCreate={handleCreate}
          onJoin={handleJoin}
          createdCode={roomCode}
          mock={isMock}
          onCheckUpdate={handleCheckUpdate}
          checkingUpdate={checkingUpdate}
        />
        {updateModal}
      </>
    );
  }

  const srcDims = selectedSourceDims();
  const shareLive = snapshot?.share.state === "live";

  return (
    <>
      <RoomScreen
      roomCode={roomCode}
      nickname={nickname.trim() || "Convidado"}
      snapshot={snapshot}
      roster={roster}
      selfId={selfId}
      selfNickname={nickname.trim()}
      watching={watching}
      quality={{
        shareLive,
        busy,
        effective,
        backend,
        backendNote,
        applying,
        applyError,
        resolution,
        onResolution: setResolution,
        customW,
        onCustomW: setCustomW,
        customH,
        onCustomH: setCustomH,
        quality,
        onQuality: setQuality,
        customBitrate,
        onCustomBitrate: setCustomBitrate,
        customFps,
        onCustomFps: setCustomFps,
        srcDims,
        onApply: handleApplyQuality,
      }}
      linkStats={linkStats}
      source={source}
      onSource={setSource}
      sources={sources}
      sourcesError={sourcesError}
      sourcesDenied={sourcesDenied}
      caps={caps}
      onListSources={handleListSources}
      previews={previews}
      onPreviewsVisible={handlePreviewsVisible}
      busy={busy}
      error={error}
      lastSignal={lastSignal}
      lastMedia={lastMedia}
      stats={stats}
      onRefresh={() => void refresh()}
      onLeave={handleLeave}
      onShare={handleShare}
      onShareCombo={handleShareCombo}
      onStopShare={handleStopShare}
      onWatch={handleWatch}
      onUnwatch={handleUnwatch}
      audioApps={audioApps}
      audioExcluded={audioExcluded}
      onToggleAudioExclude={handleToggleAudioExclude}
      mock={isMock}
      />
      {updateModal}
    </>
  );
}
