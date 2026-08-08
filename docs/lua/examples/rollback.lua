ctx.db.execute("BEGIN IMMEDIATE")

ctx.db.execute([[
  CREATE TABLE IF NOT EXISTS lua_rollback_example (
    value TEXT NOT NULL
  )
]])

ctx.db.execute(
  "INSERT INTO lua_rollback_example(value) VALUES (?1)",
  { "该行不会提交" }
)

error("演示未捕获错误只回滚当前仍打开的事务")
