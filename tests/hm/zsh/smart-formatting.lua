return {
  home = { user = "hm-user" },
  programs = {
    zsh = {
      enable = true,
      set_options = {
        "AUTO_LIST", "AUTO_PARAM_SLASH", "AUTO_PUSHD", "ALWAYS_TO_END", "CORRECT", "HIST_FCNTL_LOCK", "HIST_VERIFY",
        "INTERACTIVE_COMMENTS", "MENU_COMPLETE", "PUSHD_IGNORE_DUPS", "PUSHD_TO_HOME", "PUSHD_SILENT", "NOTIFY",
        "PROMPT_SUBST", "MULTIOS", "NOFLOWCONTROL", "NO_CORRECT_ALL", "NO_HIST_BEEP", "NO_NOMATCH",
      },
      history = {
        size = 50000,
        save = 50000,
        append = false,
        ignore_dups = false,
        ignore_all_dups = false,
        save_no_dups = false,
        find_no_dups = false,
        ignore_space = false,
        expire_duplicates_first = false,
        extended = false,
        share = false,
      },
    },
  },
}
