# TODO

## 未来考虑事项

- [x] 浅层复杂列类型开发清单：
  - [x] catalog 类型映射保存 DuckDB 原生类型，不把复杂列存成 JSON。
  - [x] DDL 支持 `<simple_type>[]`、`STRUCT(field <simple_type>, ...)`、`MAP(<simple_type>, <simple_type>)`。
  - [x] 禁止复杂类型嵌套复杂类型，错误信息明确指向 nested complex type。
  - [x] 禁止复杂列作为主键、唯一键、索引列、外键、分区键和非 `NULL` 默认值。
  - [x] Web Console 与 MySQL 兼容查询结果将复杂值统一序列化为 JSON。
  - [x] 测试覆盖建表、插入、查询、JSON 展示、快照保存恢复和限制类报错。
- [ ] 增量写快照升级时重新评估 shutdown 快照一致性边界：当前定时快照和 Web 手工快照作为阶段性备份，不阻塞写队列；未来实现增量写快照时，再考虑是否为 shutdown 场景增加 write queue drain/barrier，确保已进入写队列的写入在关闭快照前完成。
- [ ] MySQL protocol 实现必须复用 `SqlValue` typed row：prepared statement binary result 对标量类型直接按类型编码；DuckDB `LIST` / `STRUCT` / `MAP` / `ARRAY` / `UNION` / `VARIANT` / `INTERVAL` / `TIMETZ` 没有 MySQL binary protocol 原生集合类型，统一暴露为 JSON-compatible 文本或 MySQL `JSON` 类型元数据，保留嵌套值内容，但不声明成 MySQL 原生集合类型。
- [ ] MySQL protocol 开发清单见 [mysql_protocol_todo.md](mysql_protocol_todo.md)。
