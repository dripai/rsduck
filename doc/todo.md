# TODO

## 未来考虑事项
- [ ] 增量写快照升级时重新评估 shutdown 快照一致性边界：当前定时快照和 Web 手工快照作为阶段性备份，不阻塞写队列；未来实现增量写快照时，再考虑是否为 shutdown 场景增加 write queue drain/barrier，确保已进入写队列的写入在关闭快照前完成。
