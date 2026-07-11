# RSDuck 项目规则

## 任务推进

- 对已确认任务维护状态并按顺序持续执行；仅在会实质改变产品行为、数据策略、外部接口或安全边界时询问。

## Catalog 与分区

- 受管 DDL 必须经过明确的 catalog mutation；不得绕过 `rsduck_catalog` 直接修改受管对象。
- `rsduck_catalog`、逻辑表或视图、`rsduck_internal` 物理分区、依赖关系和 journal 必须在同一事务内保持一致。
- 受管分区表只允许通过逻辑表操作；不得从外部直接修改 `rsduck_internal` 下的物理分区。
- 涉及分区结构变更时，必须同步处理活跃物理分区、逻辑入口视图和分区路由元数据。
