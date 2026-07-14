current todos.

* in the todo means its a high priority
~ in the todo means its half-done/in-progress
x in the todo means its done
? in the todo means i cant recreate the issue
/ in the todo means its not too important
- in the todo means i couldnt fix it yet and will revisit later
empty brackets means its untouched

done todos get erased shortly after.

issues:
[/] - opening spotify_player.exe and playing a song freezes :resources
[?/] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[/-] - opening youtube with ublock origin activated makes it open in a half-open half-not state.
       CONFIRMED cause: uBOLite's `ublock-filters` main-world SCRIPTLET (its YouTube fetch-response
       rewrite / #reloadxhr player-fetch trick), NOT its network/DNR blocking. Only fires on the
       logged-in profile + certain videos; page hangs at readyState=loading so the player boots but
       metadata/comments never hydrate. Reload works (served from cache). Fix when revisiting: make
       uBOL skip YouTube (rename the youtube.com entries in rulesets/scripting/scriptlet/main/
       ublock-filters.js $scriptletHostnames$ -- index-preserving) and let native ADBLOCK_JS json-prune
       handle YT ads. NOTE: a separate, worse variant (netblock stalling ALL YT videos in default mode)
       was already fixed -- netblock is now gated to :adblock native only.
[x] - if im on a terminal tab with long contents and i press ctrl+s to scroll up and then press escape to go into term mode, the camera isnt brought down to the latest line, instead the camera gets stuck where it was, i have to press ctrl+s and keep pressing "j" to bring the camera down and then press esc to return to normal

feats:
[*] - :save/saved or :favorite/favorites for saving a page for later

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
[~] - improve :wq, currently it only brings back webview2 tabs accurately, terminal tabs just get restarted (if possible, should maintain the folder it was before with the content inside exactly as it was before)
      DONE: cwd restore (incl. WSL). Two sources: OSC 9;9 / OSC 7 shell-integration reports (cmd
      auto-injected via PROMPT; pwsh/WSL need a one-line prompt hook; nu optional via
      $env.config.shell_integration.osc9_9 = true), PLUS a no-setup fallback that reads the live
      process cwd of the innermost process under the pty-host at :w time (proc_cwd.rs — covers
      nu/cmd/nested shells, since they sync their physical cwd on cd; NOT pwsh, NOT inside WSL).
      :w saves it, restore reopens the shell there INVISIBLY (nu -e / pwsh -NoExit -Command /
      cmd /K carry the cd or `wsl -d <distro> --cd <path>` re-entry; unknown shells fall back to
      typing it). U-reopen keeps cwd too. WSL needs the ~/.bashrc UNC hook (__browser_cwd).
      DROPPED by choice: restoring the visible screen contents (static snapshot, not worth the
      complexity — the restored cwd is the real state).
