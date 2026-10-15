-- Ported from Home Manager's modules/programs/zsh (MIT); see tests/hm/PORTED for the commit. Nix-only parts are left out.
local t = archix.types
local f = archix.formats

local plugins = "/usr/share/zsh/plugins"
local keymaps = { emacs = "bindkey -e", viins = "bindkey -v", vicmd = "bindkey -a" }

local function quote(v)
  return '"' .. tostring(v):gsub('[\\"`]', "\\%0") .. '"'
end

local function array(items)
  local quoted = {}
  for i, item in ipairs(items) do quoted[i] = f.shell_arg(item) end
  return "\n  " .. table.concat(f.wrap_lines(quoted, 78), "\n  ") .. "\n"
end

local function sorted_pairs_lines(map, render)
  local out = {}
  for k, v in pairs(map) do out[#out + 1] = render(k, v) end
  return table.concat(out, "\n")
end

local function as_list(v)
  if type(v) == "string" then return { v } end
  return v
end

return {
  options = {
    programs = {
      zsh = {
        enable = archix.option { type = t.bool, default = false, description = "Install zsh and manage ~/.zshrc." },
        package = archix.option { type = t.nullable(t.str), default = "zsh" },
        enable_completion = archix.option { type = t.bool, default = true },
        completion_init = archix.option { type = t.lines, default = "autoload -U compinit && compinit" },
        default_keymap = archix.option { type = t.nullable(t.enum { "emacs", "viins", "vicmd" }), default = archix.null },
        cdpath = archix.option { type = t.list_of(t.str), default = {} },
        autocd = archix.option { type = t.nullable(t.bool), default = archix.null },
        autosuggestion = {
          enable = archix.option { type = t.bool, default = false },
          strategy = archix.option { type = t.list_of(t.str), default = { "history" } },
          highlight = archix.option { type = t.nullable(t.str), default = archix.null },
        },
        syntax_highlighting = {
          enable = archix.option { type = t.bool, default = false },
          highlighters = archix.option { type = t.list_of(t.str), default = {} },
          styles = archix.option { type = t.attrs_of(t.str), default = {} },
          patterns = archix.option { type = t.attrs_of(t.str), default = {} },
        },
        history_substring_search = {
          enable = archix.option { type = t.bool, default = false },
          search_up_key = archix.option { type = t.any, default = { "^[[A" } },
          search_down_key = archix.option { type = t.any, default = { "^[[B" } },
        },
        history = {
          size = archix.option { type = t.int, default = 10000 },
          save = archix.option { type = t.nullable(t.int), default = archix.null, description = "Defaults to size." },
          path = archix.option { type = t.nullable(t.str), default = archix.null, description = "Defaults to ~/.zsh_history." },
          ignore_patterns = archix.option { type = t.list_of(t.str), default = {} },
          append = archix.option { type = t.bool, default = false },
          ignore_dups = archix.option { type = t.bool, default = true },
          ignore_all_dups = archix.option { type = t.bool, default = false },
          save_no_dups = archix.option { type = t.bool, default = false },
          find_no_dups = archix.option { type = t.bool, default = false },
          ignore_space = archix.option { type = t.bool, default = true },
          expire_duplicates_first = archix.option { type = t.bool, default = false },
          extended = archix.option { type = t.bool, default = false },
          share = archix.option { type = t.bool, default = true },
        },
        set_options = archix.option { type = t.list_of(t.str), default = {} },
        shell_aliases = archix.option { type = t.attrs_of(t.str), default = {} },
        shell_global_aliases = archix.option { type = t.attrs_of(t.str), default = {} },
        dir_hashes = archix.option { type = t.attrs_of(t.str), default = {} },
        session_variables = archix.option { type = t.attrs_of(t.any), default = {} },
        init_content = archix.option { type = t.lines, default = "", description = "Extra lines for ~/.zshrc; other modules add their shell integration here." },
        env_extra = archix.option { type = t.lines, default = "" },
        profile_extra = archix.option { type = t.lines, default = "" },
        login_extra = archix.option { type = t.lines, default = "" },
        logout_extra = archix.option { type = t.lines, default = "" },
      },
    },
  },
  config = function(cfg)
    local zsh = cfg.programs.zsh
    if not zsh.enable then return {} end
    local home = archix.home_directory(cfg)
    local history = zsh.history
    local fragments = { "typeset -U path cdpath fpath manpath" }

    if #zsh.cdpath > 0 then
      local entries = {}
      for i, entry in ipairs(zsh.cdpath) do entries[i] = quote(entry) end
      fragments[#fragments + 1] = "cdpath+=(" .. table.concat(entries, " ") .. ")\n"
    end
    if zsh.default_keymap ~= nil then
      fragments[#fragments + 1] = "# Use " .. zsh.default_keymap .. " keymap as the default.\n" .. keymaps[zsh.default_keymap] .. "\n"
    end
    if zsh.enable_completion then fragments[#fragments + 1] = zsh.completion_init end
    if zsh.autosuggestion.enable then
      local strategy = #zsh.autosuggestion.strategy > 0
          and ("ZSH_AUTOSUGGEST_STRATEGY=(" .. table.concat(zsh.autosuggestion.strategy, " ") .. ")\n")
        or ""
      local highlight = zsh.autosuggestion.highlight ~= nil
          and ('ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE="' .. zsh.autosuggestion.highlight .. '"\n')
        or ""
      fragments[#fragments + 1] = "source " .. plugins .. "/zsh-autosuggestions/zsh-autosuggestions.zsh\n" .. strategy .. highlight .. "\n"
    end

    local ignore = ""
    if #history.ignore_patterns > 0 then
      ignore = "HISTORY_IGNORE=" .. f.shell_arg("(" .. table.concat(history.ignore_patterns, "|") .. ")")
    end
    local path = history.path or (home .. "/.zsh_history")
    if not path:find("$", 1, true) and path:sub(1, 1) ~= "/" then path = home .. "/" .. path end
    fragments[#fragments + 1] = "# History options should be set in .zshrc and after oh-my-zsh sourcing.\n"
      .. "# See https://github.com/nix-community/home-manager/issues/177.\n"
      .. 'HISTSIZE="' .. history.size .. '"\n'
      .. 'SAVEHIST="' .. (history.save or history.size) .. '"\n'
      .. ignore .. "\n"
      .. 'HISTFILE="' .. path:gsub("/$", "") .. '"\n'
      .. 'mkdir -p "$(dirname "$HISTFILE")"\n'

    local flags = {
      APPEND_HISTORY = history.append,
      HIST_IGNORE_DUPS = history.ignore_dups,
      HIST_IGNORE_ALL_DUPS = history.ignore_all_dups,
      HIST_SAVE_NO_DUPS = history.save_no_dups,
      HIST_FIND_NO_DUPS = history.find_no_dups,
      HIST_IGNORE_SPACE = history.ignore_space,
      HIST_EXPIRE_DUPS_FIRST = history.expire_duplicates_first,
      SHARE_HISTORY = history.share,
      EXTENDED_HISTORY = history.extended,
    }
    if zsh.autocd ~= nil then flags.autocd = zsh.autocd end
    local options = { "HIST_FCNTL_LOCK" }
    for name, enabled in pairs(flags) do
      if enabled then options[#options + 1] = name end
    end
    for name, enabled in pairs(flags) do
      if not enabled then options[#options + 1] = "NO_" .. name end
    end
    for _, option in ipairs(zsh.set_options) do options[#options + 1] = option end
    fragments[#fragments + 1] = "# Set shell options\nset_opts=(" .. array(options) .. ")\n"
      .. 'for opt in "${set_opts[@]}"; do\n  setopt "$opt"\ndone\nunset opt set_opts\n'

    if zsh.init_content ~= "" then fragments[#fragments + 1] = zsh.init_content end

    local aliases = sorted_pairs_lines(zsh.shell_aliases, function(k, v) return "alias -- " .. f.shell_arg(k) .. "=" .. f.shell_arg(v) end)
    local globals = sorted_pairs_lines(zsh.shell_global_aliases, function(k, v) return "alias -g -- " .. f.shell_arg(k) .. "=" .. f.shell_arg(v) end)
    if aliases ~= "" or globals ~= "" then
      local separator = (aliases ~= "" and globals ~= "") and "\n" or ""
      fragments[#fragments + 1] = aliases .. separator .. globals
    end
    if next(zsh.dir_hashes) then
      fragments[#fragments + 1] = "# Named Directory Hashes\n"
        .. sorted_pairs_lines(zsh.dir_hashes, function(k, v) return "hash -d " .. f.shell_arg(k) .. "=" .. quote(v) end)
        .. "\n"
    end
    if zsh.syntax_highlighting.enable then
      local highlighters = { "main" }
      for _, h in ipairs(zsh.syntax_highlighting.highlighters) do
        if h ~= "main" then highlighters[#highlighters + 1] = h end
      end
      for i, h in ipairs(highlighters) do highlighters[i] = f.shell_arg(h) end
      fragments[#fragments + 1] = "source " .. plugins .. "/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh\n"
        .. "ZSH_HIGHLIGHT_HIGHLIGHTERS=(" .. table.concat(highlighters, " ") .. ")\n"
        .. sorted_pairs_lines(zsh.syntax_highlighting.styles, function(k, v) return "ZSH_HIGHLIGHT_STYLES[" .. f.shell_arg(k) .. "]=" .. f.shell_arg(v) end)
        .. "\n"
        .. sorted_pairs_lines(zsh.syntax_highlighting.patterns, function(k, v) return "ZSH_HIGHLIGHT_PATTERNS+=(" .. f.shell_arg(k) .. " " .. f.shell_arg(v) .. ")" end)
        .. "\n"
    end
    if zsh.history_substring_search.enable then
      local up, down = {}, {}
      for _, key in ipairs(as_list(zsh.history_substring_search.search_up_key)) do up[#up + 1] = 'bindkey "' .. key .. '" history-substring-search-up' end
      for _, key in ipairs(as_list(zsh.history_substring_search.search_down_key)) do down[#down + 1] = 'bindkey "' .. key .. '" history-substring-search-down' end
      fragments[#fragments + 1] = "source " .. plugins .. "/zsh-history-substring-search/zsh-history-substring-search.zsh\n"
        .. table.concat(up, "\n") .. "\n" .. table.concat(down, "\n") .. "\n"
    end

    local exports = sorted_pairs_lines(zsh.session_variables, function(k, v) return "export " .. k .. "=" .. quote(f.nix_string(v)) end)
    local files = {
      [".zshrc"] = { text = table.concat(fragments, "\n") },
      [".zshenv"] = { text = (exports ~= "" and (exports .. "\n") or "") .. zsh.env_extra },
    }
    if zsh.profile_extra ~= "" then files[".zprofile"] = { text = zsh.profile_extra } end
    if zsh.login_extra ~= "" then files[".zlogin"] = { text = zsh.login_extra } end
    if zsh.logout_extra ~= "" then files[".zlogout"] = { text = zsh.logout_extra } end

    local packages = {}
    if zsh.package ~= nil then packages[#packages + 1] = zsh.package end
    if zsh.autosuggestion.enable then packages[#packages + 1] = "zsh-autosuggestions" end
    if zsh.syntax_highlighting.enable then packages[#packages + 1] = "zsh-syntax-highlighting" end
    if zsh.history_substring_search.enable then packages[#packages + 1] = "zsh-history-substring-search" end
    return { home = { files = files }, packages = packages }
  end,
}
