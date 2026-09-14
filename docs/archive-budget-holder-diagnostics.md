# 共享归档预算慢锁诊断

`archive_budget_acquire` 继续区分连接取得时间 `pool_wait_ms` 与预算锁查询等待 `budget_wait_ms`，新增 `operation`、`request_id`、`backend_pid`。PID 来自原有锁查询的附加投影，不增加数据库查询；SQLite 为 null。

`archive_budget_hold` 在观察到取得锁之后开始计时，持有时间达到 250ms 或提交/显式回滚失败时输出一次 WARN。字段包括 `hold_ms`、`slowest_phase`、`slowest_phase_ms`、`last_phase`、`outcome` 与相同关联标识。不逐块输出成功 INFO，不增加后台采样。

请求准入阶段区分 `key_account_reservation`、`request_record`、`archive_capture`、`event_cursor`、`commit`。财务预留内部原有 SQLx 慢日志继承请求 ID/PID，可进一步区分 key budget 与账户更新。缓冲响应终态另有 owner、conversation、settlement 与事实投影阶段；writer/GC 同样记录独立持锁区间。

将持有区间（日志时间减 `hold_ms`）与相同 PID 的 PostgreSQL blocker 快照、其他请求的 `budget_wait_ms` 交叉核对。阶段时间包含 SQL 等待与该阶段的本地工作，不能仅凭阶段名断言具体 blocker。`committed`/`rolled_back` 表示观察到相应返回成功；`commit_failed` 不能推定未提交。`abandoned` 包括取消、提前返回与未显式回滚的错误，只表示观测所有者退出，不声称服务器已释放锁或回滚 ACK。

诊断不改变事务、锁顺序、预留上限、账务原子性、重试或取消语义。不输出正文、凭据或 SQL 参数。
