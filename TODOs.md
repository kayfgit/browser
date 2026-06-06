[] - :shell should see if the given shell actually exists before applying
[] - command bar freezes while loading content
[] - autocomplete based on history, if i have accessed youtube.com recently and write ":open yout-" it should show the auto complete, which i can press tab to accept the autocomplete.
[] - look for a way to make the browser consume the least amount of resources from the pc, even with the heavy engine running
[] - maybe add tmux functionality? could be cool
[] - maybe add a :engine command to change browser engine? might be overkill
[] - make own dedicated terminal to use as little resources as possible
[~] - add the same vim motions of :error and :resources on :version and :te (on the terminal it should activate while exiting passthrough mode)  [:version DONE (now a native vim pager); :te terminal vim-on-passthrough-exit still TODO]
[~] - pressing v with a site opened like youtube.com enters visual mode, puts the cursor on the middle of the screen and allows highlighting and yanking of text with vim motions (should work on :read, :open and :research)  [:read DONE (native caret/visual select + y yank, Esc exits); :open/:research (WebView2 caret browsing via injected JS) still TODO]
[x] - always shows "[error] v select * y yank * yi( inner ()" in the command bar for some reason  [the [error] hint was shown for ANY vim-buffer tab incl. :res; now keyed to the actual tab (browser://error)]
[] - :read only accepts urls. if no url is provided, search on google (links selectable by hint mode)
[] - i tested going ":research wikipedia" to go to the wikipedia site, was met by a captcha, which i couldnt complete because the checkmark wasnt there (probably because it was deactivated while making it render only the necessary for research)
