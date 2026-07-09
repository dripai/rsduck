# MySQL Protocol 开发清单

本文记录 rsduck 增加 MySQL wire protocol 连接入口的开发清单。原则：MySQL protocol 只做入口层；认证、SQL 执行、权限、typed result、snapshot、catalog mutation 继续复用现有公共路径。

## 设计规则

- [x] MySQL server 入口必须和 PG / Web 一样调用 `DbHandle`，不得绕过 catalog、权限和 SQL router。
- [x] 认证必须走统一 `AuthRequest` / `BlockingAuthenticator` 接口，不在 MySQL handler 中直接读写 `rsduck_catalog.rs_user`。
- [x] 查询结果必须复用 `SqlTypedResult` / `SqlValue`，MySQL text result 和 prepared statement binary result 只负责协议编码。
- [x] `COM_QUERY`、`COM_STMT_PREPARE`、`COM_STMT_EXECUTE` 的 SQL 执行必须共用同一条 `DbHandle::execute_typed_sql_with_params_as` 路径。
- [x] 不增加隐式 fallback：不支持的 MySQL packet、capability、auth plugin、type codec 必须返回明确错误。

## 阶段 1：协议入口骨架

- [x] 新增 `src/server/mysql/` 模块，拆分为 `listener.rs`、`handshake.rs`、`auth.rs`、`command.rs`、`codec.rs`、`stmt.rs`、`session.rs`、`types.rs`。
- [x] 在配置中增加 `mysql.enabled`、`mysql.bind`，默认不影响现有 PG / Web。
- [x] 在 `main.rs` 中按配置启动 MySQL listener，并接入现有 shutdown 流程。
- [x] 实现连接 session 状态：connection id、capabilities、username、database、charset、prepared statement cache。
- [x] 实现 packet framing：3-byte payload length、1-byte sequence id、multi-packet 读写边界。

## 阶段 2：Handshake 与认证

- [x] 实现 MySQL initial handshake packet，明确 server version、connection id、capability flags、auth plugin name、nonce。
- [x] 解析 handshake response packet：capabilities、max packet size、charset、username、database、auth response、connection attrs。
- [x] 通过 `AuthRequest` 进入统一认证接口。
- [x] 不根据客户端版本或客户端名称猜测认证方式；服务端 handshake 固定声明 `caching_sha2_password`，客户端按该插件响应。
- [x] 连接属性中的 `_client_name` / `_client_version` / `program_name` 仅用于日志和诊断，不用于选择认证插件。
- [x] 如果客户端不支持 `CLIENT_PLUGIN_AUTH` 或不能响应 `caching_sha2_password`，返回明确认证协议不支持错误，不自动降级。
- [x] MySQL 初始认证插件固定使用 `caching_sha2_password`，匹配 MySQL 8 默认认证方式；Navicat 等客户端会按 server handshake 中声明的插件响应。
- [x] catalog 保留 Argon2 作为 Web / PG 认证 verifier，同时为 MySQL 维护独立 verifier；`AuthRequest` 根据 `AuthProtocol::MySqlWire` 调用 MySQL verifier，不复用 cleartext/Argon2 校验。
- [x] `CREATE USER` / `ALTER USER PASSWORD` 在拿到明文密码的 mutation 阶段同时生成 Argon2 verifier 和 MySQL `caching_sha2_password` verifier。
- [x] 对缺少 MySQL verifier 的历史用户返回明确认证失败，并要求重置密码生成 MySQL verifier。
- [x] `mysql_native_password` 不作为第一版默认认证方式；如后续必须兼容旧客户端，再作为显式配置项增加，不做自动降级。
- [x] 认证失败统一返回 MySQL error packet，并记录 `rsduck_audit`。

## 阶段 3：基础命令

- [x] 支持 `COM_QUERY`，将 SQL 交给公共执行路径。
- [x] 支持 `COM_PING`、`COM_INIT_DB`、`COM_QUIT`。
- [x] 支持最小 session statements：`SET autocommit`、`SELECT @@version_comment`、`SELECT DATABASE()` 等常见探测。
- [x] 未支持命令返回明确 `ER_UNKNOWN_COM_ERROR` 或 rsduck 自定义错误消息。
- [x] 实现 OK packet、ERR packet、EOF / OK terminator 的版本策略。

## 阶段 4：Text Resultset

- [x] 从 `SqlColumn` 映射 MySQL column definition metadata。
- [x] 从 `SqlValue` 编码 MySQL text row。
- [x] 支持 NULL bitmap/text NULL 表达。
- [x] 覆盖标量类型：BOOL、整数、浮点、DECIMAL、VARCHAR/TEXT、BLOB、DATE、TIME、DATETIME/TIMESTAMP、UUID。
- [x] Web / PG / MySQL 对同一 SQL 返回的行数、NULL 和文本展示保持一致。

## 阶段 5：Prepared Statement

- [x] 支持 `COM_STMT_PREPARE`：解析 SQL、生成 statement id、返回 parameter count 和 column count。
- [x] 准备阶段通过公共 describe 路径获取 `SqlColumn` metadata。
- [x] 支持 `COM_STMT_EXECUTE`：解析 flags、iteration count、null bitmap、new params bound flag、parameter types、parameter values。
- [x] 将 MySQL binary params 转成 `SqlParam`，再走公共参数绑定路径。
- [x] 支持 `COM_STMT_CLOSE`、`COM_STMT_RESET`、`COM_STMT_SEND_LONG_DATA` 的明确策略。

## 阶段 6：Prepared Binary Result

- [x] `COM_STMT_EXECUTE` 返回 binary row resultset。
- [x] 从 `SqlValue` 直接编码 binary result，禁止再把标量值转成字符串后反解析。
- [x] 支持 MySQL binary row null bitmap。
- [x] 支持标量 binary codec：TINY/SHORT/LONG/LONGLONG、FLOAT/DOUBLE、NEWDECIMAL、VAR_STRING/BLOB、DATE、TIME、DATETIME/TIMESTAMP。
- [x] UUID 默认按字符串或 `VAR_STRING` 返回，除非后续定义 MySQL 侧专用展示类型。
- [x] DECIMAL 使用稳定文本字节编码到 `MYSQL_TYPE_NEWDECIMAL`，保持精度。

## 阶段 7：复杂类型策略

- [x] DuckDB `LIST` / `STRUCT` / `MAP` / `ARRAY` / `UNION` / `VARIANT` 在内部保留为 `SqlValue::Json`。
- [x] MySQL metadata 对复杂类型统一声明为 `MYSQL_TYPE_JSON`，不做客户端探测分支。
- [x] Text result 返回 JSON 字符串。
- [x] Binary result 对 JSON 使用 length-encoded string payload。
- [x] `INTERVAL`、`TIMETZ` 没有 MySQL 原生等价类型，按稳定文本类型暴露。
- [x] 增加复杂类型集成测试：LIST、STRUCT、MAP、ARRAY 嵌套 NULL 和嵌套标量。

## 阶段 8：客户端兼容测试

- [x] rsduck 协议级 TCP 集成测试：handshake、`caching_sha2_password`、text query、复杂类型 metadata、prepared statement binary result。
- [ ] Rust MySQL 客户端 text query 测试。
- [ ] Rust MySQL 客户端 prepared statement 参数绑定和 binary result 测试。
- [ ] Python MySQL 客户端连接、查询、prepared/parameterized query 测试。
- [ ] DBeaver / Navicat 基础连接和表结构探测测试。
- [ ] 与现有 PG `tokio-postgres` 集成测试并行运行，确保 PG 能力不回退。

## 阶段 9：文档与发布

- [x] 更新 `README.zh-CN.md` 和 `README.md` 的协议入口说明。
- [x] 文档明确 MySQL protocol 是兼容入口，不改变内部 PostgreSQL-compatible catalog object model。
- [x] 文档明确 MySQL 复杂类型暴露策略。
- [x] 文档明确当前支持的 auth plugin、prepared statement、binary result 范围。
- [x] 发布前执行 `cargo fmt`、`cargo check`、`cargo test`、UTF-8 扫描。

## 已确认策略

- [x] MySQL 复杂类型 metadata 固定使用 `MYSQL_TYPE_JSON`。
- [x] MySQL listener 默认端口使用 `13306`，避免与本机 MySQL `3306` 冲突。
- [x] 第一版 MySQL protocol 不支持 TLS；文档明确仅建议本机或可信内网使用。
