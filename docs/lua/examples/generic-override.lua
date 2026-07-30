assert(ctx.project.engine == "generic")

ctx.translation.set(
  {
    group_id = assert(arg[1], "arg[1] 必须是 Group ID"),
    unit_id = assert(arg[2], "arg[2] 必须是 Unit ID"),
  },
  assert(arg[3], "arg[3] 必须是目标译文")
)
