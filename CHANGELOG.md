# changelog

## v0.7.20

- fix: webcam macOS em pipeline AVFoundation nativo (remove nokhwa do backend macOS) — delegate com entrega por callback, BGRA com stride padding, autorização por start sem prompt na enumeração
- fix: preview de câmera com registro pendente antes da readiness, promoção atômica e teardown fora do lock; falhas de runtime/interrupção observadas com erro tipado
- fix: bundle macOS com NSCameraUsageDescription + entitlement de câmera; PreviewPlayer sem leak de listener

## v0.7.19

- fix: streamar + webcam ao mesmo tempo — Compartilhar espera o preview liberar a câmera (botão desabilita em voo) em vez de falhar ocupado; `selfview_start` distingue share sintético de parado
- test: `camera_preview_share_selfview_flow` (sala real + webcam real) trava a ordem preview → share → self-view

## v0.7.18

- feat: tile "Você" no palco espelha o share ativo (prévia local, sem reabrir dispositivo); ícone Ocultar + "Mostrar meu vídeo" com pref persistido; comandos `selfview_start`/`selfview_stop`, mesmo fio GLP2 do player

## v0.7.17

- feat: preview ao vivo no modal Compartilhar (webcam contínua ~8fps, tela em stills ~1.4fps, só com o app em foco); comandos `preview_start`/`preview_stop`, mesmo fio GLP2 do player, para antes de compartilhar

## v0.7.16

- feat: detectar webcam — `camera:<id>` transmite só a webcam, `combo:display:<id>+camera:<cid>` (ou `combo:window:…`) compõe tela + webcam no canto num feed só; modal Compartilhar ganha a aba Webcams, a fonte Webcam e o PiP "tela + webcam"; segunda instância assiste sem protocolo novo

## v0.7.15

- feat: persist privacy-safe, aggregated rendezvous diagnostics in a Docker volume for troubleshooting signaling failures
- docs: track the intermittent multi-viewer screen freeze while its cause remains unconfirmed

## v0.7.14

- fix: windows share shows the mouse cursor on display capture
- fix: windows window share no longer draws the colored capture border
- fix: windows share captures each heard app in isolation (experimental); muted apps are never captured, so they cannot leak back choppy
- fix: windows share mix no longer stalls when an app goes quiet, removing the random audio pop

## v0.7.12

- fix: muted apps are left out of the windows share entirely; the rest of the audio stays normal

## v0.7.11

- fix: windows share audio is one continuous stream again (no chopped mix)

## v0.7.10

- fix: windows screen share no longer echoes when both peers share, and discord is not mixed back in
- fix: windows display share shows the mouse cursor
- fix: windows window share no longer draws the capture border
- fix: "ignorar áudio de apps" shows which apps are playing sound

## v0.7.9

- feat: player volume can boost to 200%; double-click resets to 100%
- fix: open update downloads in the OS browser, limited to this repository's GitHub release URLs

## v0.7.8

- feat: persist "seu nick" and "servidor" on the user machine (localStorage), restored on next open
- feat: "ignorar áudio de apps" blocks discord and any discord audio source by default (user can untoggle during the stream)
- feat: "ignorar áudio de apps" blocks goDrinking (app + helper) own audio by default (user can untoggle during the stream)
- ci: remove application regression tests workflow (verified locally via `python3 scripts/verify.py`)
