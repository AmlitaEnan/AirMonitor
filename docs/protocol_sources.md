# AirMonitor Verified Open-Source Wireless HID Battery Protocol Sources

This document records the exact upstream repositories, source files, licenses, verified protocol mechanics, and explicit coverage/gap notes for reference wireless HID hardware. All entries in [`docs/protocol_catalog.toml`](protocol_catalog.toml) were extracted directly from the fetched upstream files listed below—no hex payloads, VID/PIDs, or byte offsets were guessed or extrapolated. (For Apple AirPods / Beats BLE proximity advertisement parsing, see `src/wireless/bluetooth.rs`, which follows the community-documented Apple Continuity `0x004C` / `0x07` format popularized by OpenPods.)

---

## 1. Upstream Repositories Inspected

| Repository | License | Branch | Inspected Files | Role in Catalog |
| --- | --- | --- | --- | --- |
| [`Thirt927/rapoo-vt3s-autoswitch`](https://github.com/Thirt927/rapoo-vt3s-autoswitch) | MIT | `main` | `LICENSE`, `docs/PROTOCOL.md`, `src/rapoo_autoswitch/protocol.py`, `profiles.py`, `device.py` | Rapoo Gen2 Nordic 54L passive `0x07` report layout & PID matrix (`1412`/`4612` VT3, VT3S, VT7, VT3 MAX) |
| [`Ferris-echo/FerrisPulse`](https://github.com/Ferris-echo/FerrisPulse) | GPL-3.0 | `main` | `LICENSE`, `docs/HARDWARE_PROTOCOLS.md`, `src/Hardware/RapooProtocol.cs`, `RapooDeviceHelper.Status.cs`, `CompxWirelessKeyboardHelper.cs`, `BuiltInDrivers.cs` | Cross-verification of Rapoo `0x07`, CompX `05AC:024F`, MCHOSE 4D/GLW, ATK/VXE/VGN, ASUS ROG, Razer, Logitech HID++ |
| [`zbndev/rapoo_battery`](https://github.com/zbndev/rapoo_battery) | GPL-2.0 | `main` | `LICENSE`, `README.md`, `rapoo-battery.py` | Rapoo Legacy `0xBB` 7-byte report (`1215`, `1244`, `1417`) |
| [`krisgfx9/Wireless-Device-Battery-Monitor`](https://github.com/krisgfx9/Wireless-Device-Battery-Monitor) | GPL-3.0 | `main` | `LICENSE`, `docs/rapoo.md`, `src/devices.cpp`, `src/drivers/{drivers.h,rapoo.cpp,rapoo_protocol.h,sonix.cpp,akko.cpp,rongyuan.cpp,cherry.cpp,dareu.cpp,zenith.cpp,logitech.cpp,razer.cpp,steelseries.cpp,corsair.cpp,hyperx.cpp,rog.cpp,generic.cpp,kysona.cpp,glorious.cpp,atk_keyboard.cpp,aula.cpp,mchose.cpp,mchose_keyboard.cpp,mchose_modern.cpp}` | Primary verified Windows HID catalog across Rapoo, SONiX, RongYuan, Cherry, Akko, Dareu, CompX/Kysona/Zenith, MCHOSE, ATK/AULA, Logitech, Razer, SteelSeries, Corsair, HyperX, ASUS ROG |
| [`Fan4Metal/mouse_tray`](https://github.com/Fan4Metal/mouse_tray) | Unknown (no LICENSE file in repo root) | `master` | `README.md`, `mouse_tray/drivers/{driver.py,hidpp.py}`, `chipset/{nordic52.py,nordic54.py,realtek.py}`, `vendor/{attackshark.py,lamzu.py,ninjutso.py,razer.py,logitech.py,bloody.py}` | CompX Nordic 52840 (incl. Dareu A950 Air/Wing), Nordic 54L15 (`0x72/0x07`), Realtek XOR (`5253:1020`), Lamzu (`373E`/`37B0`), Ninjutso (`1915:AE1C`), Attack Shark X3 (`1D57:FA60`) |
| [`Sapd/HeadsetControl`](https://github.com/Sapd/HeadsetControl) | GPL-3.0 (GitHub API metadata) | `master` | `lib/device_registry.cpp`, `lib/devices/{corsair_void_rich.hpp,corsair_virtuoso_xt.hpp,hyperx_cloud_2_wireless.hpp,hyperx_cloud_3.hpp,razer_blackshark_v2_pro_ps.hpp,logitech_gpro_x2_lightspeed.hpp,steelseries_arctis_nova_7.hpp,steelseries_arctis_nova_pro_wireless.hpp}` | Wireless headset VID/PIDs and HID protocols (Corsair Void/Virtuoso, HyperX Cloud II/III, SteelSeries Arctis/Nova, Logitech G Pro X 2, Razer BlackShark V2 Pro PS) |
| [`qu1ncky/BatteryTray`](https://github.com/qu1ncky/BatteryTray) | MIT | `main` | `LICENSE`, `Providers/RongyuanDongleProvider.cs`, `Providers/AulaMchoseProvider.cs` | RongYuan 2.4G receiver allowlist (81 VID:PID pairs) and `0xF7` status report format |
| [`dniminenn/sharkfin`](https://github.com/dniminenn/sharkfin) | GPL-3.0 | `master` | `LICENSE`, `docs/PROTOCOL.md`, `app/src-tauri/src/protocol/receiver.rs`, `app/src-tauri/data/devices.json` | Hardware-verified (`[HW]`) RongYuan `0xF7` receiver status packet layout and Akko/Royuan device metadata |

> **License Note**: AirMonitor extracts **protocol interoperability facts** (VID/PIDs, HID Usage Pages, report IDs, byte offsets, checksum formulas) rather than copying GPL source code. For MIT-licensed references (`rapoo-vt3s-autoswitch`, `BatteryTray`), attribution is noted above.

---

## 2. Highlights & Verification Status for Reference Hardware

### 2.1 Rapoo VT3 (`24AE:1412` / `4612`, Nordic 54L) — **VERIFIED**
- **Sources**: `Thirt927/rapoo-vt3s-autoswitch/docs/PROTOCOL.md` (MIT), `krisgfx9/.../src/drivers/rapoo.cpp`, `rapoo_protocol.h`, `docs/rapoo.md`, `Ferris-echo/FerrisPulse/docs/HARDWARE_PROTOCOLS.md`.
- **Critical Protocol Fact**: Rapoo Gen2 Nordic 54L mice do **not** use an active request-response query for battery. They **passively push** Report ID `0x07` on `UsagePage = 0xFF00`, `Usage = 0x0002` (`in_len = 19` on VT3 `1412`/`4612`; `in_len = 16` on VT9 Gen 2 `1414`; `in_len = 13` or `19` on VT3 Max `1417`) roughly every 3–10 seconds.
- **Byte Offsets (with Report ID `0x07` at byte `0`)**:
  - `byte[1] & 0x0F`: link (`0` = 2.4G receiver, `1` = Bluetooth, `2` = USB cable)
  - `byte[7]`: battery state (`1` = discharging, `2` = charging; `0` = invalid/transition)
  - `byte[8]`: battery percentage (`0..100`)
  - `bytes[13..14]` (when `len >= 15`): mouse PID (little-endian)
- **Implementation Note**: Handles on `FF00:0002` should stay open across polls (or wait ~5s on first open) because `TX [07 01]` does not trigger `0x07` status frames; during live `airmonitor-probe` testing, an initial `IN[0x07]: 02 01 07 ...` frame was observed separately from the periodic `07 <conn> <gear> ... <state> <pct>` push.
- **Legacy VT3 Pro (`24AE:1215`/`4415`), VT1 Pro Max (`1233`/`4433`), VT3 Pro Max (`1244`)**: passive 7-byte `BB B0` report on `FF00:0002` (`byte[5]` state `1`/`2`, `byte[6]` `0..100` or `0x7F` unknown while charging).

### 2.2 SKN 青龙 4.0 (`362D:D101`) — **NOT IN UPSTREAM REPOS (GAP NOTED)**
- **Search Result**: Searched all 8 fetched repositories (`krisgfx9`, `FerrisPulse`, `mouse_tray`, `HeadsetControl`, `rapoo-vt3s-autoswitch`, `rapoo_battery`, `BatteryTray`, `sharkfin`). **None** contain `VID 362D` or `PID D101` or `SKN`.
- **Related Verified Protocol**: `krisgfx9/src/drivers/sonix.cpp` and `FerrisPulse/src/Hardware/CompxWirelessKeyboardHelper.cs` document the **SONiX 2.4G keyboard dongle** protocol on **`Interface 3 (MI_03)`, `UsagePage = 0xFF60`, `Usage = 0x0061`, `in_len = 33`, `out_len = 33`** (`05AC:024F`, `0C45:FEFE`, `0C45:FDFD`):
  - `TX`: `[00][20 01 00 ... sum(bytes[1..31])]` -> `00 20 01 00 ... 21` (33 bytes)
  - `RX` (with Report ID `0x00`): `[00][20][01][00][pct]` (`pct` at byte `4`, or byte `3` if Report ID `0` is stripped)
- **Gap / Risk**: Because `362D:D101` (`SKN Wireless Link`) exposes the identical `MI_03 UP=0xFF60 U=0x0061 len=(33/33/0)` descriptor on the host PC, `sonix.cpp` is the closest structural match, **but** `362D:D101` itself is unverified upstream and may require a different sub-command or wake/typing state to answer. Do not treat `362D:D101` payload offsets as upstream-verified.

### 2.3 Dareu A950 Series & Dareu A87 (`260D`) — **PARTIAL MATCH**
- **Verified in `Fan4Metal/mouse_tray` (`chipset/nordic52.py`)**:
  - **Dareu A950 Air**: `260D:1175` (2.4G dongle), `260D:1193` (USB cable)
  - **Dareu A950 Wing**: `260D:1154` (2.4G dongle), `260D:1196` (USB cable)
  - Protocol: CompX / Nordic 52840 on `FF02:0002`, 17-byte report ID `0x08`, `TX = 08 04 00 ... 49` (checksum `0x55 - sum(0..15)`), `RX` `[6]=pct`, `[7]=wired/charging`, `[8..9]=mV`.
- **Verified in `krisgfx9/src/drivers/dareu.cpp`**:
  - **Dareu EM901X**: `260D:1074` (2.4G) / `260D:1090` (USB) — split collections `FF02:0002` (17B Feature `08 04 ... 49`) + `FF01:0000` (17B Input `09 04 ... [6]=pct [7]=chg`).
  - **Dareu A980 Pro (Togran)**: `260D:1131` (2.4G) / `260D:1148` (USB) — `000C:0001` 65B Feature `0x0A`.
  - **Dareu A84 Keyboard**: `260D:0022` — `0001:0006` iface 0 65B Feature `00 04 F6 ...`, `r[5]=pct`.
- **Gap**: Exact PIDs `362D:0201` (currently labeled `Dareu A950 Pro` in `devices.toml`) and `260D:1058`/`1059` (`Dareu A87`/`A87 Pro`) were **not** present in any of the 8 fetched upstream repositories.
  - `260D:0022` (**Dareu A84 Wireless Keyboard**): Verified in `krisgfx9/src/drivers/dareu.cpp` (`0001:0006` 65B Feature `00 04 F6 ...`, `r[5]=pct`); mapped explicitly in `devices.toml` to `protocol = "sonix"` so it no longer falls into `[[vendor]]` `260D -> compx`.
  - `362D:0201` (**Pending Hardware Verification**): Not found in upstream repos; retained as legacy/community `[[device]]` placeholder (`protocol = "compx"`) and awaits live USB/HID descriptor & packet capture confirmation.
  - `260D:1058` / `260D:1059` (**Pending Hardware Verification**): Not found in upstream repos; classic Dareu A87 variants may have no wireless battery HID interface at all (see `AGENTS.md §4.3`).

### 2.4 Akko 5108B & RongYuan (`3151`) — **PARTIAL MATCH**
- **Verified in `krisgfx9/src/drivers/rongyuan.cpp`, `qu1ncky/BatteryTray/Providers/RongyuanDongleProvider.cs` (MIT), `dniminenn/sharkfin/docs/PROTOCOL.md`**:
  - RongYuan 2.4G receivers use `Interface 2`, `UsagePage = 0xFFFF`, `Usage = 0x0002`, `feat_len >= 7` (typically `65`), sending Feature `[00 F7 00 ...]` and reading back `[00][reply_ready][kb_pct][mouse_pct][kb_offline][mouse_offline][can_send][kind]` (`offline == 0` means online).
  - Verified Akko/MonsGeek/RongYuan entries upstream: `3151:5006` (Akko Air 01), `3151:5038` (MonsGeek 2.4G), `3151:4011` (RongYuan/Attack Shark K86 receiver), `3151:4014`, `3151:4017`, `3151:4021`, `3151:4027`, `3151:402D`, `3151:4038`, plus **Akko V9 Dash Pro** mouse `38EE:000C` (`krisgfx9/src/drivers/akko.cpp`, `FF1C:0092` 64B `04 ... 1A`).
- **Gap**: `3151:4015` (currently labeled `Akko 5108B` in `devices.toml`) is mentioned in `krisgfx9/src/drivers/generic.cpp` (comment citing OpenRGB #4074 as a USB ID for Attack Shark K86 alongside `3151:4011`, with BLE `3151:4013`) and in `sharkfin/devices.json` (`id: 4015` is an internal DB ID, while Akko `5108S` has `vendorId: 1121 = 0x0461, productId: 16387 = 0x4003`), but `3151:4015` is **not** in `RongyuanDongleProvider.cs`'s 81-receiver allowlist. `3151:4015` and `3151:4010` remain **Pending Hardware Verification**.

### 2.5 Cherry MX 10.1 (`046A:01A1`) — **PARTIAL MATCH**
- **Verified in `krisgfx9/src/drivers/cherry.cpp`**:
  - **CHERRY MX 3.0S Wireless**: `046A:00EA` (2.4G dongle, `min_poll_secs = 600` to preserve sleep), `046A:00E3` (USB cable) — `UsagePage = 0xFF1C`, `Usage = 0x0092`, 64-byte Input/Output, Report ID `0x04`, type `0x1A` (`[0]=04, [1..2]=sum(bytes[3..63]) LE, [3]=1A, [4]=03, [7]=55`), reply `[0]=04, [3]=1A, [8]=pct (0..100), [9]!=0` charging (enforced via `rx_match = [[3, 0x1A]]`). Also cites `046A:01E4` (CHERRY PIXIU 75 from `B1Chat/BatteryWatch`).
  - **Cherry XTRFY 1K Mouse Receiver**: `046A:0330` — CompX 17B `0x08/0x04` on `FF02:0002`.
- **Gap**: Exact PID `046A:01A1` (`Cherry MX 10.1`) was **not** found in any of the 8 fetched upstream repositories; retained as placeholder with `min_poll_secs = 600` and marked **Pending Hardware Verification** (unknown whether it uses RongYuan `FFFF:0002` `0xF7` or Cherry `FF1C:0092` `0x04/0x1A`).

---

## 2A. Removed Unverified Blind-Probe Write Frames & Read-Only Safety Policy

To satisfy `AGENTS.md §4.2 & §4.4` (strictly read-only probing; no unverified TX frames):
1. **Removed unverified TX frames in `src/wireless/protocols/sonix.rs`**:
   - `[0x00, 0xB2, 0x00, 0x00]` (unverified Keychron/SKN link-state TX on `FF60:0061`)
   - `[0x00, 0x08, 0x00, 0x07, 0x00]` (unverified VIA custom channel read TX on `FF60:0061`)
   - `[0x00, 0xA4, 0x00, 0x00]` (unverified Keychron `KC_GET_BATTERY` TX on `FF60:0061`)
   - `[0x04, 0x00, 0x00, 0x00]`, `[0x07, 0x01, 0x00, 0x00]`, `[0x08, 0x04, 0x00, 0x00]`, `[0x13, 0x00, 0x00, 0x00]` (unverified blind write probes on non-QMK `FF00+` collections)
   - Restricted `0001:0006` `00 04 F6` feature probe and `FF02:0002` `13 4A` probe to exact verified VID:PIDs instead of broad VID-only checks.
2. **Removed unverified TX frames in `src/wireless/protocols/rongyuan.rs`**:
   - `[0x06, 0x02, 0x00, 0x00]` (`write_output` blind probe on non-`FFFF:0002` collections).
3. **`[[vendor]]` fallback read-only guard (`src/wireless/protocols/mod.rs`)**:
   - Any unknown PID matched only via `[[vendor]]` is wrapped in `ReadOnlyHidIo` inside `probe_protocol()`, blocking all `write_output` and `set_feature` calls and permitting only `GetFeature`, `GetInputReport`, and passive `ReadFile` inspection.
4. **External `[[custom_hid]]` write gate (`src/wireless/protocols/custom.rs`, `mod.rs`)**:
   - External `devices.toml` rules default to read-only (`passive_input` / `get_feature`). Active write rules require explicit `allow_write = true`, pass a dangerous VIA (`FF60`: `0x03,0x05,0x06,0x07,0x09,0x0A,0x0B,0x0D,0x0F,0x10,0x13,0x15`) and HID++ (`SET_REGISTER` / `DFU` / non-read `IRoot`) opcode blacklist, and pass TX length bounds checks against the target HID report length.

---

## 3. Protocol Group Reference (Extracted Byte Formats)

1. **Rapoo Gen2 Nordic 54L (`rapoo`)**: Passive input `0x07` on `FF00:0002` (`19B`/`16B`/`13B`); `[1]&0x0F`=link, `[7]`=state (`1`=discharging, `2`=charging), `[8]`=`%`.
2. **Rapoo Legacy (`rapoo`)**: Passive input `0xBB 0xB0` on `FF00:0002` (`7B`); `[5]`=state (`1`/`2`), `[6]`=`%` (`0x7F`=unknown while charging).
3. **CompX / Nordic 52840 (`compx`)**: `FF02:0002` `17B` Out/In; TX `08 04 00 ... 49` (`checksum = 0x55 - sum(0..15)`), RX `[6]`=`%`, `[7]`=charging/wired, `[8..9]`=mV BE.
4. **CompX / Nordic 54L15 (`Fan4Metal/mouse_tray`)**: `FF05:0001` `64B` Out/In; TX `08 7D 72 02 00 <00 cable|01 dongle> 07 01 ...`, RX `[1]=72 [2]=00 [5]=07 [7]=%`.
5. **SONiX 2.4G Keyboard (`sonix`)**: `FF60:0061` iface 3 `33B` Out/In; TX `00 20 01 00 ... 21`, RX `[0..3]=00 20 01 00 [4]=%` (or `[0..2]=20 01 00 [3]=%`).
6. **RongYuan 2.4G Receiver (`rongyuan`)**: `FFFF:0002` iface 2 `65B` Feature; TX `00 F7 00 ...`, RX `[2]=kb% [3]=ms% [4]=kb_off [5]=ms_off`.
7. **Cherry / Akko V9 (`FF1C:0092`)**: `64B` Out/In; TX `04 72 00 1A 03 00 00 55 ...`, RX `[0]=04 [3]=1A [8]=% [9]=chg`.
8. **Evision ATK/VGN (`320F:5088`/`5055`)**: `FF1C:0092` `32B` Out/In; TX `04 <seq16> 1A <00|06> ...`, RX `[0]=04 [1..2]=seq [3]=1A [8]=% [9]=chg(1|2)`.
9. **Glorious / Lamzu / Hitscan / WLMouse**: `FFFF:0000` iface 2 `65B` Feature; TX `00 00 00 02 02 00 83 ...`, RX `[1]=A1 [6]=83 [7]=chg [8]=%`.
10. **AULA / MCHOSE "BY" (`3554:FA09`, `41E4:2001`)**: `FF02:0002` `20B` Out/In; TX `13 4A 00 ... 5D` (or `01 ... 5E`), RX `[0]=13 [1]&0x7F=4A [5]=% [6]>>4=chg`.
11. **MCHOSE 4D (`3837:1014/1018/4028`)**: `FF01:0001` `64B` Out/In; TX `4D 01 01 00 00 09 00 00 09 ...`, RX `[19]=chg [20]=%`.
12. **MCHOSE GLW (`3837:3033`)**: `0001:0000` / `FF31:0074` `65B` Out/In; TX `00 55 05 00 38 38 ...`, RX `[0]=AA [1]=05 [40]` bit7=chg, bits0..6=`%` (or `+1` if RID 0 present).
13. **MCHOSE Realtek (`5253:1020`, `3837:100C`)**: `FF01:0001` `65B` Feature; TX `12 F9 FF ...`, RX `[0..1]=12 F9`, `[11]^FF=%`, `[12]^FF=chg`.
14. **Ninjutso Sora V2 (`1915:AE1C`)**: `FFA0:0001` `32B` Feature ID `0x05`; TX `05 15 00 00 01 ...`, RX `[0]=05 [9]=% [10]=chg`.
15. **Attack Shark X3 (`1D57:FA60`)**: `000A:0000` iface 2 passive input Report `0x03` (~2s push); RX `[0]=03 [4]=level (0..10)`.
16. **Logitech HID++ (`046D`)**: `FF00:0002` (`20B` RID `0x11` / `7B` RID `0x10`), dynamic feature discovery implemented for `0x1004` UnifiedBattery (`[4]=% [6]=chg`) and `0x1000` BatteryLevelStatus (upstream also documents `0x1001` / `0x1F20` headset mV, not yet implemented in `hidpp.rs`).
17. **Headsets (SteelSeries `1038`, Corsair `1B1C`, HyperX `03F0`/`0951`, ASUS ROG `0B05`)**: Exact per-model rules recorded in `docs/protocol_catalog.toml`.
