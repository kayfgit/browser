[] - still has that issue of making any commands not work and only getting control back by alt tabbing
[] - :shell should see if the given shell actually exists before applying
[] - remember last session
[] - command bar freezes while loading content
[] - add bangs
[] - allow math on command bar for easy quick maths, for example ":20*8" shows the result inline like "160", allowing copying or continuing the maths like "160+10"
[x] - join all of the browser process in one, in task manager its all over the place (still doesnt work, maybe because im using "cargo run -p browser-desktop"?)
[] - autocomplete based on history, if i have accessed youtube.com recently and write ":open yout-" it should show the auto complete, which i can press tab to accept the autocomplete.
[] - look for a way to make the browser consume the least amount of resources from the pc, even with the heavy engine running
[x] - search just like qutebrowser (press / and type, like /example highlights all the words "example" on the page, n goes forward/N goes backward, should work on all modes including read)
[x] - ctrl-w, should erase the last word in the url, not the entire url. same for ctrl+arrowkeys
[] - issue when using ":open", it says "failed to open: WebView2 error: WindowsError(Error { code: HRESULT(0x8007139f), message: 'O grup})" and i cant read the rest because its all inline.
[] - maybe add tmux functionality? could be cool
