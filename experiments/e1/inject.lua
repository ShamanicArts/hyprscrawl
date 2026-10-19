-- scrawl E1 probe: injected with hyprctl eval 'dofile(...)'
scrawl_probe = scrawl_probe or {}
if scrawl_probe.subs then for _, s in ipairs(scrawl_probe.subs) do pcall(function() s:remove() end) end end
if scrawl_probe.f then pcall(function() scrawl_probe.f:close() end) end
local f = io.open("$PROBE_DIR/events.log", "a"); f:setvbuf("line"); scrawl_probe.f = f
scrawl_probe.subs = {}
local function w(s) f:write(s, "\n") end
local function pos() local p = hl.get_cursor_pos(); if p then return string.format("%.1f %.1f", p.x, p.y) end return "nil nil" end
local function desc(v)
  local t = type(v)
  if t == "table" then local o = {} for k, x in pairs(v) do o[#o+1] = tostring(k) .. "=" .. tostring(x) end return "{" .. table.concat(o, ",") .. "}" end
  return t .. ":" .. tostring(v)
end
local function shift() local ok, r = pcall(hl.is_key_down, "Shift_L"); return tostring(ok and r) end
table.insert(scrawl_probe.subs, hl.on("input.keyboard.key", function(...)
  local a = {...}; local o = {} for i = 1, select("#", ...) do o[#o+1] = desc(a[i]) end
  w("K " .. table.concat(o, " ") .. " shiftdown=" .. shift() .. " pos=" .. pos())
end))
local opts = { non_consuming = true, ignore_mods = true, transparent = true, submap_universal = true }
local function with(t, extra) local r = {} for k, v in pairs(t) do r[k] = v end for k, v in pairs(extra or {}) do r[k] = v end return r end
for _, b in ipairs({ "mouse:272", "mouse:273", "mouse:274" }) do
  table.insert(scrawl_probe.subs, hl.bind(b, function() w("B " .. b .. " down " .. pos()) end, opts))
  table.insert(scrawl_probe.subs, hl.bind(b, function() w("B " .. b .. " up " .. pos()) end, with(opts, { release = true })))
end
for _, b in ipairs({ "mouse_up", "mouse_down", "mouse_left", "mouse_right" }) do
  table.insert(scrawl_probe.subs, hl.bind(b, function() w("W " .. b .. " " .. pos()) end, opts))
end
w("INJECTED subs=" .. #scrawl_probe.subs)
