local PRIO_FORCE, PRIO_DEFAULT = ...
local OPTION, TYPE, PRIO = {}, {}, {}
local NULL = setmetatable({}, {
  __tostring = function() return "archix.null" end,
  __newindex = function() error("archix.null is read-only", 2) end,
})

local function ty(kind, fields)
  fields = fields or {}
  fields[TYPE] = kind
  return fields
end

local function expect_type(t, fname)
  if type(t) ~= "table" or rawget(t, TYPE) == nil then
    error(fname .. ": expected an archix type such as archix.types.str", 3)
  end
  return t
end

local types = {
  bool = ty("bool"),
  int = ty("int"),
  float = ty("float"),
  str = ty("str"),
  path = ty("path"),
  lines = ty("lines"),
  any = ty("any"),
}

function types.enum(values)
  if type(values) ~= "table" or #values == 0 then error("types.enum: expected a list of strings", 2) end
  for _, v in ipairs(values) do
    if type(v) ~= "string" then error("types.enum: expected a list of strings", 2) end
  end
  return ty("enum", { values = values })
end

function types.list_of(t) return ty("list", { elem = expect_type(t, "types.list_of") }) end
function types.attrs_of(t) return ty("attrs", { elem = expect_type(t, "types.attrs_of") }) end
function types.nullable(t) return ty("nullable", { elem = expect_type(t, "types.nullable") }) end

function types.submodule(options)
  if type(options) ~= "table" then error("types.submodule: expected a table of options", 2) end
  return ty("submodule", { options = options })
end

local option_keys = { type = true, default = true, description = true }

local function option(spec)
  if type(spec) ~= "table" then error("archix.option: expected a table", 2) end
  for k in next, spec do
    if not option_keys[k] then error("archix.option: unknown field '" .. tostring(k) .. "'", 2) end
  end
  expect_type(spec.type, "archix.option")
  return { [OPTION] = true, type = spec.type, default = spec.default, description = spec.description }
end

local function override(priority, value)
  if math.type(priority) ~= "integer" then error("archix.override: priority must be an integer", 2) end
  if value == nil then error("archix.override: value is nil", 2) end
  return { [PRIO] = priority, value = value }
end

local type_rank = { number = 1, string = 2, boolean = 3 }

local function key_less(a, b)
  if a == b then return false end
  local ra, rb = type_rank[type(a)], type_rank[type(b)]
  if not (ra and rb) then
    if ra or rb then return ra ~= nil end
    error("pairs: two keys are tables or functions, which have no reproducible order; key the table by strings or numbers", 3)
  end
  if ra ~= rb then return ra < rb end
  if type(a) == "boolean" then return (not a) and b end
  return a < b
end

-- Lua's hash order is seeded per run; sorted keys keep evaluation reproducible.
function pairs(t)
  if type(t) ~= "table" then error("bad argument #1 to 'pairs' (table expected, got " .. type(t) .. ")", 2) end
  local mt = getmetatable(t)
  if mt and mt.__pairs then return mt.__pairs(t) end
  local keys = {}
  for k in next, t do keys[#keys + 1] = k end
  table.sort(keys, key_less)
  local i = 0
  return function()
    i = i + 1
    local k = keys[i]
    if k ~= nil then return k, rawget(t, k) end
  end, t, nil
end

-- A file spec downloaded from url and checked against sha256, for files and home.files.
local function fetchurl(spec)
  if type(spec) ~= "table" or type(spec.url) ~= "string" then
    error("archix.fetchurl needs { url = ..., sha256 = ... }", 2)
  end
  if type(spec.sha256) ~= "string" or not spec.sha256:match("^%x+$") or #spec.sha256 ~= 64 then
    error("archix.fetchurl: sha256 must be 64 hex digits; `archix prefetch " .. spec.url .. "` prints it", 2)
  end
  local out = {}
  for k, v in next, spec do out[k] = v end
  return out
end

local function secret_name(name, fname)
  if type(name) ~= "string" or not name:match("^[%w_%-]+$") or name == "files" then
    error(fname .. ": expected a secret's name from secrets.items (letters, digits, - and _)", 3)
  end
  return name
end

archix = {
  -- A placeholder the deployment's boot replaces with the value; evaluation never sees it.
  secret = function(name) return "{{archix-secret:" .. secret_name(name, "archix.secret") .. "}}" end,
  secret_path = function(name) return "/run/archix/secrets/" .. secret_name(name, "archix.secret_path") end,
  fetchurl = fetchurl,
  types = types,
  option = option,
  override = override,
  force = function(value) return override(PRIO_FORCE, value) end,
  default = function(value) return override(PRIO_DEFAULT, value) end,
  null = NULL,
}

return { option = OPTION, type = TYPE, prio = PRIO, null = NULL }
