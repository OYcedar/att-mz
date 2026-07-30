ctx.db.execute([[
  CREATE TABLE IF NOT EXISTS lua_notes (
    key  TEXT PRIMARY KEY,
    note TEXT NOT NULL
  )
]])

local key = assert(arg[1], "arg[1] 必须是 key")
local note = assert(arg[2], "arg[2] 必须是 note")

ctx.db.execute(
  "INSERT INTO lua_notes(key, note) VALUES (?1, ?2) " ..
  "ON CONFLICT(key) DO UPDATE SET note = excluded.note",
  { key, note }
)
