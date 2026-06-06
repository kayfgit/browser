[] - :shell should see if the given shell actually exists before applying
[] - autocomplete based on history, if i have accessed youtube.com recently and write ":open yout-" it should show the auto complete, which i can press tab to accept the autocomplete.
[] - look for a way to make the browser consume the least amount of resources from the pc, even with the heavy engine running
[] - maybe add tmux functionality? could be cool
[] - maybe add a :engine command to change browser engine? might be overkill
[] - make own dedicated terminal to use as little resources as possible
[~] - add the same vim motions of :error and :resources on :version and :te (on the terminal it should activate while exiting passthrough mode)  [:version DONE (now a native vim pager); :te terminal vim-on-passthrough-exit still TODO]
[x] - pressing v with a site opened like youtube.com enters visual mode, puts the cursor on the middle of the screen and allows highlighting and yanking of text with vim motions (should work on :read, :open and :research)  [DONE on all three: :read native caret; :open/:research via injected CARET_JS (Selection.modify + caret bar, y yanks over IPC, Esc collapses then exits)]
[x] - change default screen to just say "browser - lightweight modal shell" in a h1 or something  [welcome screen is now just the centered name + tagline + one hint line; long keybind list removed (still in :commands)]
[x] - currently :search needs the whole url to set the engine, like ":search https://duckduckgo.com/?q=%s", it should be just ":search duckduckgo" or ":search ddg"  [:search now accepts a bang-table engine name (ddg/google/wiki/yt/…) or a %s/URL template]
[FIX] - :read <query> was broken (readability can't parse a SERP). now routes plain queries through the DDG-lite search backend → clean followable results doc
[] - same visual issue as before, cursor is slightly offset, scales with how far to the right it is and zoom
[] - make top bar (where the links sit) draggable with a mouse, just a quality of life.
