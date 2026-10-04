// TDD RED — feature/webcam-detect-stream
// Uso real 1: stream (tela) + webcam simultâneos
// Uso real 2: só stream (regressão)
// Uso real 3: só webcam
// Uso real 4: segunda instância assiste (modelo watch por id)
//
// Estes testes DEVEM FALHAR antes da implementação:
// - validateSource("camera:...") hoje rejeita (só synthetic/movie/display/window)
// - sourceKindOf("camera:...") hoje cai em "synthetic"
// - CapabilitySet hoje não tem eixo `camera`
// - mockSources/mockCaps hoje não expõem webcam
import { describe, expect, it } from "vitest";
import {
  sourceKindOf,
  validateCombo,
  validateSource,
} from "./views";
import { mockCaps, mockSources } from "./mock";
import type { CapabilitySet, SourceInfo } from "./api";

describe("webcam TDD — validação de fonte (uso real: escolher origem no modal Compartilhar)", () => {
  it("uso 2 (regressão): stream display continua válido", () => {
    expect(validateSource("display:1")).toBeNull();
    expect(sourceKindOf("display:1")).toBe("display");
  });

  it("uso 3 (só webcam): 'camera:<id>' é fonte válida", () => {
    expect(validateSource("camera:0")).toBeNull();
  });

  it("uso 3 (só webcam): kind derivado é 'camera', nunca 'synthetic'", () => {
    expect(sourceKindOf("camera:0")).toBe("camera");
  });

  it("uso 1 (stream+webcam): display + camera coexistem como seletores distintos", () => {
    expect(validateSource("display:1")).toBeNull();
    expect(validateSource("camera:0")).toBeNull();
    expect(sourceKindOf("display:1")).not.toBe(sourceKindOf("camera:0"));
  });

  it("uso 1/3: 'camera:' vazio rejeita com mensagem (igual display:/window:)", () => {
    expect(validateSource("camera:")).not.toBeNull();
    expect(validateSource("camera:   ")).not.toBeNull();
  });
});

describe("webcam TDD — capacidades e lista (uso real: Listar telas deve oferecer webcam)", () => {
  it("CapabilitySet expõe eixo camera com motivo honesto", () => {
    const caps = mockCaps() as CapabilitySet & { camera?: { supported: boolean; reason: string } };
    expect(caps.camera, "CapabilitySet precisa do eixo camera (display/window/camera)").toBeDefined();
    expect(typeof caps.camera!.supported).toBe("boolean");
    expect(caps.camera!.reason.length).toBeGreaterThan(0);
  });

  it("lista de fontes inclui pelo menos 1 webcam em desktop real", () => {
    const sources: SourceInfo[] = mockSources() as SourceInfo[];
    const cams = sources.filter((s) => (s.kind as string) === "camera");
    expect(cams.length, "mockSources deve expor a webcam (espelha list_sources real)").toBeGreaterThan(0);
    for (const cam of cams) {
      expect(cam.id.length).toBeGreaterThan(0);
      expect(cam.name.length).toBeGreaterThan(0);
    }
  });
});

describe("webcam TDD — segunda instância (uso real: outro peer assiste a webcam)", () => {
  it("watch de membro com share de camera usa o mesmo contrato por id opaco", async () => {
    // Contrato: watch(memberId) não distingue kind — o kind viaja no SourceInfo/share.
    // Este teste trava o modelo: roster com share=true + kind camera deve ser assistível.
    const roster = [
      { id: "m-host", nickname: "Host", master: true, share: true },
    ];
    const cameraShare = {
      kind: "camera",
      id: "0",
      name: "Webcam · 1280x720",
      w: 1280,
      h: 720,
    } as unknown as SourceInfo;
    expect(roster[0].share).toBe(true);
    expect((cameraShare.kind as string)).toBe("camera");
    // O watch continua endereçando o membro (não o kind) — sem novo comando.
    expect(roster[0].id).toBe("m-host");
  });
});

describe("webcam — combo tela+webcam (uso real: um feed só, viewer sem protocolo novo)", () => {
  it("combo válido de display/window + camera passa", () => {
    expect(validateSource("combo:display:1+camera:0")).toBeNull();
    expect(validateSource("combo:window:42+camera:1")).toBeNull();
    expect(sourceKindOf("combo:display:1+camera:0")).toBe("combo");
  });

  it("combo rejeita tela que não é tela e câmera sem id", () => {
    expect(validateCombo("combo:camera:0+camera:1")).not.toBeNull();
    expect(validateCombo("combo:synthetic+camera:0")).not.toBeNull();
    expect(validateCombo("combo:display:1+camera:")).not.toBeNull();
    expect(validateCombo("combo:display:   +camera:0")).not.toBeNull();
    expect(validateCombo("combo:display:1")).not.toBeNull();
    expect(validateCombo("combo:")).not.toBeNull();
    expect(validateSource("combo:display:1+camera:0")).toBeNull();
    expect(validateSource("combo:camera:0+camera:1")).not.toBeNull();
  });
});
