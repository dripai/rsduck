# TODO

## 未来考虑事项
- [ ] 增量写快照升级时重新评估 shutdown 快照一致性边界：当前定时快照和 Web 手工快照作为阶段性备份，不阻塞写队列；未来实现增量写快照时，再考虑是否为 shutdown 场景增加 write queue drain/barrier，确保已进入写队列的写入在关闭快照前完成。

## 桌面端服务管理与升级（跨平台规划）

### 统一能力
- [x] 定义并实现桌面端状态模型：托盘分别呈现服务管理器状态与 `/healthz` 实际可用性，覆盖“已停止、启动中、运行中、停止中、异常”。
- [x] 实现统一的“启动、停止、重启”入口：Windows 通过 UAC + Service Control Manager，Linux 通过 polkit + `systemctl`，macOS 通过管理员授权的 `launchctl`；命令失败会在托盘状态中显示错误。
- [ ] 为优雅停止建立平台无关的验证：停止服务必须触发最终快照；无法确认最终快照完成时，操作失败并给出明确错误。
- [x] 定义并生成发布清单与升级校验：版本、目标平台、下载地址和 SHA-256 为必填项；托盘在校验失败时拒绝启动安装。
- [ ] 将“安装后可在用户未登录时自动运行”设为 Release 服务包的必备验收项：在干净环境安装、重启机器且不登录用户后，服务必须已启动并可通过 Web/MySQL 健康检查；仅包含可执行文件的归档包不得作为服务部署包。
- [x] Web SQL 服务端入口已实现：Web 控制台通过 `[web].bind` 提供，桌面端只负责打开对应地址。
- [x] 日志文件已实现：按 `[log].dir` 写入和轮转，桌面端只负责打开日志目录或定位最新日志。

### Windows
- [x] 保持 Windows 服务安装包作为正式服务部署产物；仅含 `rsduck.exe` 的 ZIP 包不承诺开机自启。
- [x] 新增独立的 `rsduck-tray.exe`，通过所有用户登录启动项自动启动；它不影响或替代 Windows Service 的开机自启。
- [x] 实现托盘菜单：状态、启动、停止、重启、检查/执行升级、打开 Web SQL、打开日志目录、退出托盘。
- [x] 通过 Windows Service Control Manager 管理服务；仅在启动、停止、重启和升级时请求 UAC 管理员权限。
- [ ] 验证 Windows Service 的 stop/preshutdown 信号能够触发 rsduck 的优雅关闭和最终快照；不能可靠触发时，补充明确的关闭信号处理与自动化验证。
- [x] 实现 Windows 安装包升级：托盘检查版本、下载并校验安装包、退出托盘后以管理员权限运行安装包；安装包停止服务、替换程序、保留配置与快照、重启服务并在下次登录启动新版托盘。

### Linux
- [x] 补充 Linux 服务部署包：包含 system-level `systemd` unit、安装/卸载脚本；安装时 `enable` 且启动服务，使 rsduck 在未登录时随机器启动。
- [x] 提供桌面环境自启动入口作为独立状态栏/托盘进程；它只连接 system service，不负责拉起或承载数据库服务进程。
- [x] 实现 Linux 桌面菜单：状态、启动、停止、重启、检查/执行升级、打开 Web SQL、打开日志。
- [ ] 通过 `systemctl` 与 polkit 管理 system service 权限；当前已调用 `pkexec systemctl`，仍需在真实桌面环境确认 polkit 授权、最终快照和错误反馈。
- [x] 定义 Linux 服务包与升级方式，并保留已有配置、快照与日志。

### macOS
- [x] 补充 macOS 服务部署包：安装并加载 system-level `launchd` daemon，使 rsduck 在未登录时随机器启动。
- [x] 提供 LaunchAgent 启动的菜单栏应用作为独立交互进程；它只连接 system daemon，不负责拉起或承载数据库服务进程。
- [x] 实现 macOS 菜单栏菜单：状态、启动、停止、重启、检查/执行升级、打开 Web SQL、打开日志。
- [ ] 通过受授权的特权 helper 管理 daemon 的启动、停止、重启和升级；当前通过管理员授权的 `launchctl` 执行，仍需实现特权 helper 并确认最终快照。
- [ ] 定义 macOS 签名、公证、发布包与升级方式，并保证升级时保留配置、快照与日志。
