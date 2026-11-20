# AutoClicker

Windows 连点器：录制鼠标与键盘操作，做成可反复回放的脚本，并用全局热键随时启停。

## 功能

- 录制鼠标点击、移动与键盘事件，生成可回放的脚本
- 脚本库：重命名、删除、回收站恢复，按 100 条分页浏览
- 连点任务：自定义点击间隔，可选随机抖动，按住模式运行中可随时停止
- 全局热键启停；鼠标甩到主屏左上角 3×3 像素区域急停
- 录制上限 100,000 事件，超出时停止扩增并提示
- 管理员权限重载、单实例运行、设置自动保存与损坏备份

## 下载

安装包见 [Releases](https://github.com/jue5hen/autoclicker/releases/latest)：

- `AutoClicker_1.0.0_x64-setup.exe` — 按当前用户安装，默认不需要管理员权限

系统要求：Windows 10/11 x64，Microsoft Edge WebView2 Runtime（Windows 11 已内置）。

## 从源码构建

前置：Node.js 20+、Rust stable、Windows 上可用的 MSVC 或 GNU 工具链、WebView2 Runtime。

```powershell
npm ci
npm run tauri build
```

产物在 `src-tauri/target/release/bundle/nsis/`。

## 许可证

[MIT](LICENSE)
