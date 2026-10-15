-- Config file writers; each mirrors the Nix generator Home Manager uses, so ported modules render the same bytes.
local formats = {}
local null = archix.null

local function keys(t)
  local list = {}
  for k in next, t do list[#list + 1] = k end
  table.sort(list, function(a, b) return tostring(a) < tostring(b) end)
  return list
end

local function is_list(v)
  if type(v) ~= "table" or v == null then return false end
  local n = #v
  if n == 0 then return false end
  for k in next, v do
    if math.type(k) ~= "integer" or k < 1 or k > n then return false end
  end
  return true
end

local function is_map(v)
  return type(v) == "table" and v ~= null and not is_list(v)
end

local function float_repr(x)
  if x ~= x then return "nan" end
  if x == math.huge then return "inf" end
  if x == -math.huge then return "-inf" end
  for precision = 15, 17 do
    local s = string.format("%." .. precision .. "g", x)
    if tonumber(s) == x then
      if not s:find("[%.eEn]") then s = s .. ".0" end
      return s
    end
  end
end

-- Nix's toString: integers as is, floats with six decimals, true as "1" and false as "".
function formats.nix_string(v)
  if type(v) == "boolean" then return v and "1" or "" end
  if math.type(v) == "float" then return string.format("%f", v) end
  return tostring(v)
end

function formats.yes_no(v) return v and "yes" or "no" end

-- Nix's escapeShellArg: safe strings stay bare, the rest get single quotes.
function formats.shell_arg(s)
  s = tostring(s)
  if s:match("^[%w,._+:@%%/%-]+$") then return s end
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

-- Home Manager's wrapLines: packs items into lines no longer than width.
function formats.wrap_lines(items, width)
  local lines, current = {}, ""
  for _, item in ipairs(items) do
    local candidate = current == "" and item or (current .. " " .. item)
    if #candidate <= width then
      current = candidate
    else
      lines[#lines + 1] = current
      current = item
    end
  end
  if current ~= "" then lines[#lines + 1] = current end
  return lines
end

local function json_string(s)
  local escapes = { ['"'] = '\\"', ["\\"] = "\\\\", ["\b"] = "\\b", ["\f"] = "\\f", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }
  return '"' .. s:gsub('[%c"\\]', function(c) return escapes[c] or string.format("\\u%04x", c:byte()) end) .. '"'
end

local function json(v, level)
  local pad = string.rep("  ", level)
  if v == nil or v == null then return "null" end
  if type(v) == "boolean" then return tostring(v) end
  if math.type(v) == "integer" then return tostring(v) end
  if math.type(v) == "float" then return float_repr(v) end
  if type(v) == "string" then return json_string(v) end
  if is_list(v) then
    local items = {}
    for _, item in ipairs(v) do items[#items + 1] = pad .. "  " .. json(item, level + 1) end
    return "[\n" .. table.concat(items, ",\n") .. "\n" .. pad .. "]"
  end
  local entries = {}
  for _, k in ipairs(keys(v)) do entries[#entries + 1] = pad .. "  " .. json_string(tostring(k)) .. ": " .. json(v[k], level + 1) end
  if #entries == 0 then return "{}" end
  return "{\n" .. table.concat(entries, ",\n") .. "\n" .. pad .. "}"
end

-- JSON as `jq .` prints it: two-space indent, keys sorted.
function formats.json(v) return json(v, 0) .. "\n" end

local function toml_string(s)
  local escapes = { ['"'] = '\\"', ["\\"] = "\\\\", ["\b"] = "\\b", ["\f"] = "\\f", ["\n"] = "\\n", ["\r"] = "\\r", ["\t"] = "\\t" }
  return '"' .. s:gsub('[%c"\\]', function(c) return escapes[c] or string.format("\\u%04X", c:byte()) end) .. '"'
end

local function toml_key(k)
  k = tostring(k)
  if k:match("^[%w_%-]+$") then return k end
  return toml_string(k)
end

local function toml_value(v)
  if type(v) == "boolean" then return tostring(v) end
  if math.type(v) == "integer" then return tostring(v) end
  if math.type(v) == "float" then return float_repr(v) end
  if type(v) == "string" then return toml_string(v) end
  if is_list(v) then
    local items = {}
    for _, item in ipairs(v) do items[#items + 1] = toml_value(item) end
    return "[" .. table.concat(items, ", ") .. "]"
  end
  if is_map(v) then
    local items = {}
    for _, k in ipairs(keys(v)) do items[#items + 1] = toml_key(k) .. " = " .. toml_value(v[k]) end
    return "{ " .. table.concat(items, ", ") .. " }"
  end
  return toml_string(tostring(v))
end

local function table_array(v)
  if not is_list(v) then return false end
  for _, item in ipairs(v) do
    if not is_map(item) then return false end
  end
  return true
end

local function toml_table(out, t, path, header)
  local scalars, tables = {}, {}
  for _, k in ipairs(keys(t)) do
    local v = t[k]
    if v ~= null then
      if is_map(v) or table_array(v) then tables[#tables + 1] = k else scalars[#scalars + 1] = k end
    end
  end
  if header and (#scalars > 0 or #tables == 0) then
    if #out > 0 then out[#out + 1] = "" end
    out[#out + 1] = header
  end
  for _, k in ipairs(scalars) do out[#out + 1] = toml_key(k) .. " = " .. toml_value(t[k]) end
  for _, k in ipairs(tables) do
    local name = path == "" and toml_key(k) or (path .. "." .. toml_key(k))
    if table_array(t[k]) then
      for _, item in ipairs(t[k]) do
        if #out > 0 then out[#out + 1] = "" end
        out[#out + 1] = "[[" .. name .. "]]"
        toml_table(out, item, name, nil)
      end
    else
      toml_table(out, t[k], name, "[" .. name .. "]")
    end
  end
end

-- TOML the way Home Manager's pkgs.formats.toml writes it.
function formats.toml(t)
  local out = {}
  toml_table(out, t, "", nil)
  return table.concat(out, "\n") .. "\n"
end

local function ini_value(v)
  if type(v) == "boolean" then return v and "true" or "false" end
  return formats.nix_string(v)
end

-- INI like lib.generators.toINI: global keys, then sorted sections; lists become repeated keys.
function formats.ini(t, opts)
  opts = opts or {}
  local sep = opts.separator or "="
  local function lines(section)
    local out = {}
    for _, k in ipairs(keys(section)) do
      local v = section[k]
      local values = is_list(v) and v or { v }
      if opts.list_separator and is_list(v) then
        local parts = {}
        for _, item in ipairs(v) do parts[#parts + 1] = ini_value(item) end
        values = { table.concat(parts, opts.list_separator) }
      end
      for _, item in ipairs(values) do out[#out + 1] = k .. sep .. ini_value(item) .. "\n" end
    end
    return table.concat(out)
  end
  local globals, sections = {}, {}
  for _, k in ipairs(keys(t)) do
    if is_map(t[k]) then sections[#sections + 1] = k else globals[k] = t[k] end
  end
  local parts = {}
  local global_lines = lines(globals)
  if global_lines ~= "" then parts[#parts + 1] = global_lines end
  for _, name in ipairs(sections) do parts[#parts + 1] = "[" .. name .. "]\n" .. lines(t[name]) end
  return table.concat(parts, "\n")
end

-- systemd unit files as Home Manager writes them.
function formats.systemd(t) return formats.ini(t) end

local function git_value(v)
  if type(v) == "string" then
    local escapes = { ["\n"] = "\\n", ["\t"] = "\\t", ['"'] = '\\"', ["\\"] = "\\\\" }
    return '"' .. v:gsub('[\n\t"\\]', escapes) .. '"'
  end
  if type(v) == "boolean" then return tostring(v) end
  return formats.nix_string(v)
end

-- git config like lib.generators.toGitINI: nested keys become [section "sub"], keys tab-indented.
function formats.git_ini(t)
  local flat = {}
  local function walk(path, v)
    if is_map(v) then
      for _, k in ipairs(keys(v)) do
        local next_path = {}
        for i, p in ipairs(path) do next_path[i] = p end
        next_path[#next_path + 1] = tostring(k)
        walk(next_path, v[k])
      end
    elseif v ~= null then
      local key = path[#path]
      local section = table.concat(path, ".", 1, #path - 1)
      flat[section] = flat[section] or {}
      flat[section][key] = v
    end
  end
  walk({}, t)
  local parts = {}
  for _, name in ipairs(keys(flat)) do
    local header = name
    local first, rest = name:match("^([^.]+)%.(.+)$")
    if first and not name:find('"', 1, true) then header = first .. ' "' .. rest .. '"' end
    local lines = { "[" .. header .. "]\n" }
    for _, k in ipairs(keys(flat[name])) do
      local v = flat[name][k]
      local values = is_list(v) and v or { v }
      for _, item in ipairs(values) do lines[#lines + 1] = "\t" .. k .. " = " .. git_value(item) .. "\n" end
    end
    parts[#parts + 1] = table.concat(lines)
  end
  return table.concat(parts, "\n")
end

-- `key value` lines (or another separator), keys sorted.
function formats.key_value(t, opts)
  opts = opts or {}
  local sep = opts.separator or " "
  local render = opts.value or formats.nix_string
  local out = {}
  for _, k in ipairs(keys(t)) do out[#out + 1] = k .. sep .. render(t[k]) .. "\n" end
  return table.concat(out)
end

local function yaml_scalar(v)
  if v == null then return "null" end
  if type(v) == "boolean" then return tostring(v) end
  if math.type(v) == "integer" then return tostring(v) end
  if math.type(v) == "float" then return float_repr(v) end
  return json_string(tostring(v))
end

local function yaml(out, v, indent)
  local pad = string.rep("  ", indent)
  if is_list(v) then
    for _, item in ipairs(v) do
      if is_map(item) or is_list(item) then
        out[#out + 1] = pad .. "-"
        yaml(out, item, indent + 1)
      else
        out[#out + 1] = pad .. "- " .. yaml_scalar(item)
      end
    end
  else
    for _, k in ipairs(keys(v)) do
      local item = v[k]
      if (is_map(item) and next(item)) or is_list(item) then
        out[#out + 1] = pad .. json_string(tostring(k)) .. ":"
        yaml(out, item, indent + 1)
      elseif is_map(item) then
        out[#out + 1] = pad .. json_string(tostring(k)) .. ": {}"
      else
        out[#out + 1] = pad .. json_string(tostring(k)) .. ": " .. yaml_scalar(item)
      end
    end
  end
end

function formats.yaml(v)
  local out = {}
  yaml(out, v, 0)
  return table.concat(out, "\n") .. "\n"
end

-- KDE kconfig: nested groups become [Group][Subgroup].
function formats.kconfig(t)
  local parts = {}
  local function group(path, g)
    local lines, subgroups = {}, {}
    for _, k in ipairs(keys(g)) do
      if is_map(g[k]) then subgroups[#subgroups + 1] = k else lines[#lines + 1] = k .. "=" .. ini_value(g[k]) end
    end
    if #lines > 0 then parts[#parts + 1] = path .. "\n" .. table.concat(lines, "\n") .. "\n" end
    for _, k in ipairs(subgroups) do group(path .. "[" .. k .. "]", g[k]) end
  end
  for _, k in ipairs(keys(t)) do
    if is_map(t[k]) then group("[" .. k .. "]", t[k]) end
  end
  return table.concat(parts, "\n")
end

local function kdl_value(v)
  if type(v) == "string" then return json_string(v) end
  if type(v) == "boolean" then return v and "#true" or "#false" end
  if v == null then return "#null" end
  if math.type(v) == "float" then return float_repr(v) end
  return tostring(v)
end

local function kdl(out, t, indent)
  local pad = string.rep("    ", indent)
  for _, k in ipairs(keys(t)) do
    local v = t[k]
    if is_map(v) then
      out[#out + 1] = pad .. k .. " {"
      kdl(out, v, indent + 1)
      out[#out + 1] = pad .. "}"
    elseif is_list(v) then
      local args = {}
      for _, item in ipairs(v) do args[#args + 1] = kdl_value(item) end
      out[#out + 1] = pad .. k .. " " .. table.concat(args, " ")
    else
      out[#out + 1] = pad .. k .. " " .. kdl_value(v)
    end
  end
end

-- KDL: tables become nodes with children, lists become arguments.
function formats.kdl(t)
  local out = {}
  kdl(out, t, 0)
  return table.concat(out, "\n") .. "\n"
end

archix.formats = formats

-- The home directory of home.user, for modules that need absolute paths in it.
function archix.home_directory(cfg)
  local user = cfg.home.user
  if user == nil then error("this needs home.user, the user whose home archix manages", 2) end
  if cfg.home.directory ~= nil then return cfg.home.directory end
  local declared = cfg.users[user]
  if declared ~= nil and declared.home ~= nil then return declared.home end
  return "/home/" .. user
end

-- A thin program module: an enable switch, settings rendered to one file in one format, and the package.
function archix.program(name)
  return function(spec)
    local writer = archix.formats[spec.format]
    if writer == nil then error("archix.program " .. name .. ": unknown format " .. tostring(spec.format), 2) end
    local t = archix.types
    return {
      options = {
        programs = {
          [name] = {
            enable = archix.option { type = t.bool, default = false, description = "Install and configure " .. name .. "." },
            package = archix.option { type = t.nullable(t.str), default = spec.package or name },
            settings = archix.option { type = t.any, default = {}, description = spec.file .. " contents, written as " .. spec.format .. "." },
            extra_config = archix.option { type = t.lines, default = "", description = "Text appended to " .. spec.file .. "." },
          },
        },
      },
      config = function(cfg)
        local program = cfg.programs[name]
        if not program.enable then return {} end
        local text = writer(program.settings)
        if program.extra_config ~= "" then text = text .. program.extra_config .. "\n" end
        local result = { home = { files = { [spec.file] = { text = text } } } }
        if program.package ~= nil then result.packages = { program.package } end
        return result
      end,
    }
  end
end
