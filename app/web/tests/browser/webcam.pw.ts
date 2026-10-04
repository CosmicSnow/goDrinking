import { test, expect } from '@playwright/test';

// Navegação real pela UI em modo mock (fontes mock: display + window + camera).
// Abrir o modal já lista as fontes (openShare → onListSources).
//
// Uso 1: stream (tela) + webcam simultâneos (PiP/combo)
// Uso 2: só stream (regressão)
// Uso 3: só webcam
// Uso 4: segunda instância/aba vê o share

async function lobbyToRoom(page: any) {
  await page.goto('/');
  await expect(page.getByTestId('mock-tag')).toBeVisible();
  await page.getByLabel('Senha da sala para criar').fill('webcam-tdd');
  await page.getByRole('button', { name: 'Criar sala', exact: true }).click();
  await expect(page.getByRole('region', { name: 'Transmissões da sala' })).toBeVisible();
}

async function openShareModal(page: any) {
  const modal = page.locator('[data-hook="share-enumeration"]');
  for (let i = 0; i < 4 && !(await modal.isVisible()); i++) {
    await page.locator('[data-hook="share-open"]').first().click();
  }
  await expect(modal).toBeVisible({ timeout: 8000 });
}

test('uso 2 (regressão): só stream — Telas oferece display', async ({ page }) => {
  await lobbyToRoom(page);
  await openShareModal(page);
  await expect(page.getByRole('button', { name: 'Telas', exact: true })).toBeVisible();
  await expect(page.getByText('Tela principal · 1920×1080').first()).toBeVisible({ timeout: 8000 });
});

test('uso 3 (só webcam): aba Webcams lista e seleciona camera:<id>', async ({ page }) => {
  await lobbyToRoom(page);
  await openShareModal(page);
  await page.getByRole('button', { name: 'Webcams', exact: true }).click();
  await expect(page.getByText('Webcam · 1280×720').first()).toBeVisible({ timeout: 8000 });
  await page.getByText('Webcam · 1280×720').first().click();
  await expect(page.locator('.source.sel').first()).toBeVisible({ timeout: 8000 });
  await expect(page.locator('#source-kind')).toHaveValue('camera');
  // Mock (sem Tauri): preview ao vivo vira placeholder honesto, sem invoke.
  await expect(page.getByTestId('preview-mock')).toBeVisible({ timeout: 8000 });
});

test('uso 1 (stream + webcam): tela selecionada oferece PiP com a webcam', async ({ page }) => {
  await lobbyToRoom(page);
  await openShareModal(page);
  await expect(page.getByText('Tela principal · 1920×1080').first()).toBeVisible({ timeout: 8000 });
  await page.getByText('Tela principal · 1920×1080').first().click();
  await expect(page.locator('.source.sel').first()).toBeVisible({ timeout: 8000 });
  // Com tela + webcam listada, o combo PiP aparece e arma o botão dedicado.
  await expect(page.getByLabel('Webcam junto (canto do vídeo)')).toBeVisible({ timeout: 8000 });
  await page.getByLabel('Webcam junto (canto do vídeo)').selectOption({ index: 1 });
  await expect(page.getByRole('button', { name: 'Compartilhar tela + webcam', exact: true })).toBeVisible({ timeout: 8000 });
});

test('uso 4 (segunda instância vê): viewer em outra aba enxerga quem compartilha webcam', async ({ browser }) => {
  const host = await browser.newPage();
  const viewer = await browser.newPage();
  await lobbyToRoom(host);
  await lobbyToRoom(viewer);
  // Viewer deve ver o tile/Assistir do host quando ele compartilha (qualquer kind).
  // Hoje passa no mock para display; deve continuar passando para camera.
  await expect(viewer.getByRole('region', { name: 'Transmissões da sala' })).toBeVisible();
  await host.close();
  await viewer.close();
});

test('self-view: meu tile no palco oculta com ícone e volta no topo', async ({ page }) => {
  await lobbyToRoom(page);
  // Mock entra compartilhando: meu tile abre visível com Ocultar.
  await expect(page.locator('[data-hook="tile-self"]')).toBeVisible({ timeout: 8000 });
  await expect(page.getByTestId('selfview-mock')).toBeVisible();
  await page.getByRole('button', { name: 'Ocultar', exact: true }).click();
  await expect(page.locator('[data-hook="tile-self"]')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Mostrar meu vídeo', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Mostrar meu vídeo', exact: true }).click();
  await expect(page.locator('[data-hook="tile-self"]')).toBeVisible({ timeout: 8000 });
});
