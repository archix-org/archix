-- Ported from Home Manager's modules/programs/git.nix (MIT); see tests/hm/cases.toml for the commit.
local t = archix.types
local f = archix.formats

local signers = { openpgp = "/usr/bin/gpg", ssh = "/usr/bin/ssh-keygen", x509 = "/usr/bin/gpgsm" }

-- Overlays b onto a, recursing into tables, like Nix's recursive merge where b's values win.
local function merge(a, b)
  local out = {}
  for k, v in pairs(a) do out[k] = v end
  for k, v in pairs(b) do
    if type(v) == "table" and v ~= archix.null and #v == 0 and type(out[k]) == "table" and #out[k] == 0 then
      out[k] = merge(out[k], v)
    else
      out[k] = v
    end
  end
  return out
end

local function set(tbl, path, value)
  for i = 1, #path - 1 do
    tbl[path[i]] = tbl[path[i]] or {}
    tbl = tbl[path[i]]
  end
  tbl[path[#path]] = value
end

return {
  options = {
    programs = {
      git = {
        enable = archix.option { type = t.bool, default = false, description = "Install and configure git." },
        package = archix.option { type = t.nullable(t.str), default = "git" },
        settings = archix.option {
          type = t.any,
          default = {},
          description = "git config as nested tables: { user = { name = ... } }; deeper levels become [section \"sub\"].",
        },
        signing = archix.option {
          type = t.nullable(t.submodule {
            key = archix.option { type = t.nullable(t.str), default = archix.null },
            format = archix.option { type = t.nullable(t.enum { "openpgp", "ssh", "x509" }), default = archix.null },
            signer = archix.option { type = t.nullable(t.str), default = archix.null },
            sign_by_default = archix.option { type = t.nullable(t.bool), default = archix.null },
            allowed_signers = archix.option { type = t.lines, default = "" },
          }),
          default = archix.null,
        },
        ignores = archix.option { type = t.list_of(t.str), default = {} },
        attributes = archix.option { type = t.list_of(t.str), default = {} },
        includes = archix.option {
          type = t.list_of(t.submodule {
            path = archix.option { type = t.str },
            condition = archix.option { type = t.nullable(t.str), default = archix.null },
          }),
          default = {},
        },
        lfs = {
          enable = archix.option { type = t.bool, default = false },
          skip_smudge = archix.option { type = t.bool, default = false },
        },
      },
    },
  },
  config = function(cfg)
    local git = cfg.programs.git
    if not git.enable then return {} end
    local derived = {}
    local signing = git.signing
    if signing ~= nil then
      if signing.key ~= nil then set(derived, { "user", "signingKey" }, signing.key) end
      if signing.sign_by_default ~= nil then
        set(derived, { "commit", "gpgSign" }, signing.sign_by_default)
        set(derived, { "tag", "gpgSign" }, signing.sign_by_default)
      end
      if signing.format ~= nil then
        set(derived, { "gpg", "format" }, signing.format)
        set(derived, { "gpg", signing.format, "program" }, signing.signer or signers[signing.format])
      end
      if signing.allowed_signers ~= "" then
        set(derived, { "gpg", "ssh", "allowedSignersFile" }, archix.home_directory(cfg) .. "/.config/git/allowed_signers")
      end
    end
    if git.lfs.enable then
      local smudge = git.lfs.skip_smudge and "git-lfs smudge --skip -- %f" or "git-lfs smudge -- %f"
      set(derived, { "filter", "lfs" }, {
        clean = "git-lfs clean -- %f",
        process = git.lfs.skip_smudge and "git-lfs filter-process --skip" or "git-lfs filter-process",
        required = true,
        smudge = smudge,
      })
    end
    local fragments = { f.git_ini(merge(derived, git.settings)) }
    for _, include in ipairs(git.includes) do
      if include.condition ~= nil then
        fragments[#fragments + 1] = f.git_ini { includeIf = { [include.condition] = { path = include.path } } }
      else
        fragments[#fragments + 1] = f.git_ini { include = { path = include.path } }
      end
    end
    local files = { [".config/git/config"] = { text = table.concat(fragments, "\n") } }
    if #git.ignores > 0 then files[".config/git/ignore"] = { text = table.concat(git.ignores, "\n") .. "\n" } end
    if #git.attributes > 0 then files[".config/git/attributes"] = { text = table.concat(git.attributes, "\n") .. "\n" } end
    if signing ~= nil and signing.allowed_signers ~= "" then
      files[".config/git/allowed_signers"] = { text = signing.allowed_signers }
    end
    local packages = {}
    if git.package ~= nil then packages[#packages + 1] = git.package end
    if git.lfs.enable then packages[#packages + 1] = "git-lfs" end
    return { home = { files = files }, packages = packages }
  end,
}
