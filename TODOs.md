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
[*] - if i zoom on a tab it changes the zoom for all the tabs, both the browser and content zoom. it should be individual (since browser zoom is related to terminal zoom, separating browser and terminal might be needed)
[*] - insert mode should also be exitable with ctrl+s or shift+escape
[] - still has that annoying issue of the commandbar being frozen but only sometimes
[x] - caret mode cursor is not placed correctly when selecting, its always one to the right. the word "apple" for example, if the cursor is on the "a" of the "apple" and i start selecting, the selection doesnt happen until i move the cursor somewhere (it should, just like vim), so if i select the word until the cursor is on "e" of the "apple" and i yank it, i only yank "appl" and not "apple"
[] - saving a session with :w sometimes doesnt work, no idea why
[] - random not responding after new update 26/07/2026 22:54
[*] - pressing a copy button using the hint mode doesnt actually copy it (tested on the copy repository button on github)
[*] - this fucking out of focus mode freeze thing needs to be fixed, every time i have to alt tab to get control back and its fucking annoying
[*] - got it now, the freeze happens when i press a button on the webview (like a chatgpt copy, or a picoctf "go to the next challenge"), it makes me unable to do anything related to the commandbar, only fix is alt tabbing.
[] - if i scroll up and start typing on claude code, the screen stays up there when its supposed to go down the second i start typing.

feats:
[] - make ";" toggle browser hud visibility
[] - add inspect and view source to right mouse button, aswell as a command and a keyboard shortcut
[] - make the "U" button undo the layout, for example if i accidently close something, or move something around, U will undo it (maybe add R to redo it aswell) 
[x] - add "s" hint mode to select scrollable stuff (like a box that inside a site that can be scrolled down), so i would press "s", hit the hint keys and scroll inside that box, pressing esc would get out of the box naturally.
[] - maybe change ":q" to ":quit"? i keep quitting the browser accidently when i want to quit vim because i think im in passthrough when im in normal
[] - add vertical sidebar like zen browser

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
