assert(ctx.project.engine == "generic")

local id = assert(arg[1], "arg[1] 必须是可读 ID")
local translation = assert(arg[2], "arg[2] 必须是目标译文")

ctx.translation.set(id, { translation })
