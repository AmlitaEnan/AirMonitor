# AirMonitor 项目级 AI Agent 开发规范 (`AGENTS.md`)

本文档为 **AirMonitor** 仓库的项目级工程规范（优先级高于全局通用规则），适用于所有参与本仓库开发、调试、调研与验证的 AI Agent。

---

## 1. 项目概览与目录结构

**AirMonitor** 是一个面向 Windows 10 / Windows 11 的超轻量级任务栏硬件监控与无线外设电量监视器（Rust 2021 + `windows` 0.58 + Direct2D / DirectWrite）。

```text
AirMonitor/
├── .cargo/
│   └── config.toml             # MSVC 静态 CRT 配置（+crt-static）
├── Cargo.toml                  # 仅依赖 serde、toml、windows 0.58（禁止引入重型运行时/GUI 框架）
├── build.rs                    # 生成并链接 Windows 资源（RT_ICON/RT_GROUP_ICON、RT_VERSION、PerMonitorV2 RT_MANIFEST）与 /DEPENDENTLOADFLAG:0x800
├── devices.toml                # 内嵌设备库（[[device]] / [[vendor]] / [[custom_hid]]），通过 include_str! 编入二进制
├── assets/
│   ├── airmonitor.ico          # Windows 多分辨率应用与托盘源图标
│   └── airmonitor.svg          # 应用矢量图标
├── docs/
│   ├── protocol_catalog.toml   # 从已核实开源仓库整理的协议目录
│   └── protocol_sources.md     # 上游开源协议来源、许可证与固件边界记录
└── src/
    ├── lib.rs                  # 库入口，导出 config / hw / state / ui / updater / wireless
    ├── main.rs                 # 主程序入口（DLL 安全搜索目录、--smoke-test 预算校验、更新重启后清理）
    ├── config.rs               # AppConfig（config.toml 便携目录 / %APPDATA%\AirMonitor，原子写 + .bak 备份 + 只读模式）
    ├── state.rs                # HardwareMetrics、WirelessDevice、MonitorSnapshot、format_speed / format_speed_full
    ├── updater.rs              # WinHTTP GitHub Releases 检查/下载、URL 白名单、CNG SHA-256、x64 PE 校验与原子替换回滚
    ├── bin/
    │   └── probe.rs            # airmonitor-probe 诊断命令行（硬件双采样 + 硬件数据源诊断 + WirelessManager::probe_diagnostics）
    ├── hw/
    │   ├── mod.rs              # HardwareCollector + HardwareDemand 按需采集调度与 30s 空闲资源释放
    │   ├── cpu.rs              # GetSystemTimes + CallNtPowerInformation + GetActiveProcessorCount + 按需 PDH CPU 频率
    │   ├── mem.rs              # GlobalMemoryStatusEx
    │   ├── net.rs              # GetIfTable2 按 InterfaceLuid 逐物理网卡流量差分
    │   ├── disk.rs             # 按需 PDH PhysicalDisk 读写速率采集与释放
    │   ├── power.rs            # GetSystemPowerStatus 本机电池电量与充电状态
    │   ├── gpu.rs              # DXGI 独显筛选 + AMD ADL2 / NVIDIA NVML / Intel IGCL + PDH 按需显存/3D占用
    │   └── sensors.rs          # 只读共享内存外部传感器融合（HWiNFO64 / AIDA64 / MSI Afterburner）+ 按需 ACPI Thermal Zone
    ├── wireless/
    │   ├── mod.rs              # WirelessManager（poll_once / probe_diagnostics / 跨传输去重）
    │   ├── xinput.rs           # LoadLibraryExW(LOAD_LIBRARY_SEARCH_SYSTEM32) 动态加载 xinput1_4.dll / xinput1_3.dll
    │   ├── bluetooth.rs        # SetupDi BT PnP 电量 + BTHLEDEVICE/BTHLE MAC 关联 + WinRT AirPods BLE 被动广播
    │   ├── hid.rs              # SetupDi HID 枚举（路径缓存）+ OVERLAPPED Read/Write & DeviceIoControl 异步封装
    │   └── protocols/
    │       ├── mod.rs          # DeviceRegistry（include_str!("../../../devices.toml") + 外部覆盖与写入闸门）
    │       ├── custom.rs       # 声明式 [[custom_hid]] 引擎（allow_write 闸门 + 危险操作码黑名单）
    │       └── {standard,rapoo,sonix,compx,rongyuan,hidpp}.rs
    └── ui/
        ├── mod.rs              # 消息循环、SnapshotStore(Arc<MonitorSnapshot>)、RefCell 重入守卫、TaskbarCreated 恢复
        ├── taskbar.rs          # 任务栏浮条定位、SetParent 子窗口管理、脏标记渲染、GeometryStabilizer 防抖、Ctrl+左键拖拽
        ├── renderer.rs         # Direct2D(SOFTWARE RT) / DirectWrite DIB 渲染、thread_local! 共享工厂与资源缓存
        ├── theme.rs            # 暗色/亮色/高对比度主题色板、状态色与 WCAG 对比度封装
        ├── text.rs             # DirectWrite TextFormat、系统 locale 回退与省略号裁剪封装
        ├── popup/
        │   ├── mod.rs          # 原生 Win11/Win10 浮窗生命周期（概览 / 设置三子页 / 关于与更新）、RefCell 分发与绘制入口
        │   ├── layout.rs       # 固定 468 DIP（半屏钳位）浮窗布局、内部滚动高度测算与 WYSIWYG 指标网格布局
        │   ├── widgets.rs      # 卡片、按钮、滚动条、更新进度条与键值行等 Direct2D 绘制组件
        │   └── actions.rs      # 浮窗交互动作、更新状态机防重入、配置保存与错误反馈
        └── tray.rs             # 系统托盘图标（NOTIFYICON_VERSION_4）、DPI 图标、低电量气泡与右键菜单
```

---

## 2. 硬性工程约束与性能预算

- **纯用户态、免驱动、免管理员权限**：禁止依赖内核驱动、WinRing0 或要求 UAC 提权（Manifest 声明 `asInvoker`）。扩展温度/功耗/风扇仅通过只读共享内存（HWiNFO64 / AIDA64 / MSI Afterburner）或标准 Windows PDH 获取。
- **零重型依赖**：严禁引入 WebView2、Electron、Qt、Tokio 等重型运行时或 UI 框架；UI 严格使用 Win32 + Direct2D / DirectWrite。
- **软件渲染策略（P-1）**：任务栏浮条、详情浮窗与托盘图标统一使用 `D2D1_RENDER_TARGET_TYPE_SOFTWARE` 配合 `thread_local!` 共享 D2D/DWrite 工厂（退出前调用 `release_thread_local_d2d()` 释放 COM 句柄），避免加载 40+ MB 硬件 GPU UMD（如 `amdxx64.dll`）导致常驻内存膨胀。
- **体积与资源预算**：
  - `cargo build --release` 下 `airmonitor.exe` 单文件体积 **< 2 MB**（当前 ~1.19 MB / `1,247,744` 字节，已配置 `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`、`+crt-static` 及内置图标/版本/PerMonitorV2 Manifest）。
  - 常驻内存 Working Set 控制在 **≤ 15 MB** 区间（SOFTWARE RT 实测约 2.6–6 MB）。
  - 空闲 CPU 占用 **< 0.5%**（当前 < 0.2%）。
  - `--smoke-test` 预算自检：自动附加父控制台（`ATTACH_PARENT_PROCESS`）并开启配置只读模式（`set_read_only_mode(true)`）；若已有实例运行则以非零退出码失败；执行采样与渲染后校验 `WorkingSet <= 15.00 MB` 与 `IdleCpuPct <= 0.50%` 并报告 `GpuUmd`。
- **无虚假数据**：采集不到数据时诚实返回 `None` / `N/A`，禁止伪造、硬编码或猜测电量/温度/速率数值。

---

## 3. Win32 / Win11 UI 与并发安全不变量

1. **任务栏子窗口托管（禁止回到 `WS_EX_TOPMOST` 顶层抢焦点循环）**：
   - 任务栏浮条必须通过 `SetParent(hwnd, taskbar_hwnd)` 挂载在 `Shell_TrayWnd` 内部（`WS_CHILD | WS_CLIPSIBLINGS`，移除 `WS_POPUP` 与 `WS_EX_TOPMOST`）。
   - 调用 `UpdateLayeredWindow` 前必须通过 `ScreenToClient` 将屏幕坐标转换为 `Shell_TrayWnd` 客户区坐标。
   - 检查 `GetParent(hwnd) == Ok(taskbar_hwnd)`，注意 Win32 `SetParent` 对原顶层窗口返回 `NULL` 在 `windows` 0.58 中会映射为 `Err`。
   - 全屏应用覆盖任务栏时依靠子窗口自然遮挡（配合 `GeometryStabilizer` 冻结全屏期间几何、过滤退出全屏瞬态位移），避免无谓 `SW_HIDE` 引发闪烁；仅任务栏自动隐藏或竖排停靠时隐藏浮条。
2. **锁外渲染/IO 与 `Arc<MonitorSnapshot>`**：
   - `SnapshotStore` 使用 `Arc<MonitorSnapshot>` 与独立版本号（`hw_rev` / `wireless_rev` / `config_rev`）；UI 线程仅在读锁内 `Arc::clone` 后立刻释放锁，禁止持锁执行 D2D 渲染、跨进程 `SetParent`/`UpdateLayeredWindow`/`Shell_NotifyIconW`、`config.save()` 或注册表读写。
3. **`RefCell` 重入安全（禁止裸指针直转 `&mut`）**：
   - `UiApp` 与 `PopupManager` 均须通过 `RefCell` + `try_borrow_mut()` 托管；调用会同步重入 `wndproc` 的 Win32 API（`TrackPopupMenu`、`SetForegroundWindow`、`SetWindowPos`、`ShowWindow`、`DestroyWindow`、`SetCapture`、`ReleaseCapture`、`TrackMouseEvent`）前必须显式释放借用，重入冲突通过 `PostMessageW` 延后处理。
4. **Explorer 重启恢复与系统集成**：
   - 必须监听 `RegisterWindowMessageW(w!("TaskbarCreated"))` 并重建/重新挂载浮条与托盘图标（含 `NIM_ADD` 有限次重试）。
   - 托盘右键菜单弹出须使用 `TPM_BOTTOMALIGN` 避让任务栏区域；`NOTIFYICON_VERSION_4` 适配键盘与 `WM_CONTEXTMENU`。
   - 文字与背景配色须读取 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\SystemUsesLightTheme`（并支持系统高对比度），按 `GetDpiForWindow` 缩放。
5. **DLL 搜索与进程安全**：
   - 进程入口（`airmonitor` 与 `airmonitor-probe`）调用 `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)`；动态加载系统/驱动 DLL（`atiadlxx.dll`、`nvml.dll`、`ControlLib.dll`/`IntelControlLib.dll`、`xinput1_4.dll`/`xinput1_3.dll`、`winhttp.dll`、`bcrypt.dll`、`kernel32.dll`）必须使用 `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)`；MSVC 链接配置 `/DEPENDENTLOADFLAG:0x800`（见 `build.rs`）。
6. **内存收敛**：
   - 窗口初始化、启动稳定后、硬件空闲释放传感器后以及浮窗关闭后调用 `trim_working_set()`（`SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX)`）保持低 Working Set。

---

## 4. 无线与 HID 安全不变量

1. **禁止同步阻塞 HID 读取**：
   - 所有设备句柄必须以 `FILE_SHARE_READ | FILE_SHARE_WRITE` + `FILE_FLAG_OVERLAPPED` 打开。
   - 所有 `ReadFile` / `WriteFile` 以及控制传输（`DeviceIoControl` 配合 `IOCTL_HID_GET_FEATURE` / `IOCTL_HID_SET_FEATURE` / `IOCTL_HID_GET_INPUT_REPORT`，禁止无超时同步 `HidD_*` 报告调用）必须使用 `OVERLAPPED` + `WaitForSingleObject` 有限超时（典型 80–500 ms），超时后必须调用 `CancelIoEx`，确保探测永不卡死 UI 或轮询线程。
2. **严格只读探测与外部写入闸门**：
   - 仅允许发送只读状态/电量查询帧。**严禁**发送任何刷写固件（DFU/OTA）、恢复出厂、重置配置、修改按键映射或写入板载 EEPROM/Flash 的报文。
   - `[[vendor]]` 厂商回退匹配一律走只读守卫（禁止向未知 PID 主动发送 `write_output` / `set_feature`）。
   - 外部 `devices.toml` 中的 `[[custom_hid]]` 规则默认只允许只读模式（`passive_input` / `get_feature` 且 `tx_hex` 为空）；任何写类规则必须显式声明 `allow_write = true`，并通过报告长度校验与危险操作码黑名单（如 VIA/QMK `0xFF60` 写/刷写指令、Logitech HID++ 写寄存器/DFU/重置子命令）。
3. **蓝牙与电源安全**：
   - 经典蓝牙 / BLE 电量读取通过 Windows PnP 属性（`DEVPKEY_Bluetooth_Battery` + `BTHLEDEVICE` ↔ `BTHLE` MAC 关联）和被动 BLE 广播（`BluetoothLEAdvertisementWatcher` + `AdvertisementFilter`）完成，禁止强行连接或劫持耳机音频通道。
   - 尊重设备休眠与无电量接口事实（如原厂 2.4G SKN 青龙 4.0、经典 Dareu A87），不得通过高频轮询阻止键盘进入休眠，遵守 `min_poll_secs` 与失败退避预算。
4. **设备库与事实真实性**：
   - `devices.toml` 通过 `include_str!("../../../devices.toml")` 内嵌并支持外部 `devices.toml` 覆盖；新条目须来自实机验证或已核实开源代码（参见 `docs/protocol_sources.md`），严禁编造 VID/PID、Report ID 或字节偏移。

---

## 5. 标准构建、测试与核验命令

在 PowerShell 中执行 Cargo 命令前，务必前置 Rust 工具链路径：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# 代码格式与静态检查（要求 0 error、0 warning）
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings

# 编译检查与单元测试（要求全部通过）
cargo check --all-targets
cargo test

# Release 构建
cargo build --release --bin airmonitor --bin airmonitor-probe

# 实机硬件与无线探测核验
.\target\release\airmonitor-probe.exe

# 主程序无头冒烟自检（核验 WorkingSet、IdleCpuPct 与采样链路）
.\target\release\airmonitor.exe --smoke-test
```
