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
[x] - :ads should only toggle the current adblocker, if i have native on and run :ads it will turn it off as intended, but running :ads again puts me in ubo and not native
[*] - insert mode should also be exitable with ctrl+s or shift+escape
[] - still has that annoying issue of the commandbar being frozen but only sometimes
[*] - caret mode cursor is not placed correctly when selecting, its always one to the right. the word "apple" for example, if the cursor is on the "a" of the "apple" and i start selecting, the selection doesnt happen until i move the cursor somewhere (it should, just like vim), so if i select the word until the cursor is on "e" of the "apple" and i yank it, i only yank "appl" and not "apple"
[] - saving a session with :w sometimes doesnt work, no idea why
[] - random not responding after new update 26/07/2026 22:54
[*] - pressing a copy button using the hint mode doesnt actually copy it (tested on the copy repository button on github)
[*] - this fucking out of focus mode freeze thing needs to be fixed, every time i have to alt tab to get control back and its fucking annoying
[x] - allow mouse selection in the terminal

feats:
[*] - :save/saved or :favorite/favorites for saving a page for later
[*] - add copy to right mouse button click
[] - make ";" toggle browser hud visibility
[x] - add profiles, like ":saveprofile work" which saves the layout, tabs, everything to a profile. then the profile can be brought back by ":profile work". also needs a way to get a clean temporary profile (just like emac's *scratch*) to test something out, sometimes theres a bunch of stuff on my screen and i just want to get rid of everything and get back a clean slate but also not lose all of the things i had opened, so it needs a command to easily get rid of everything and easily get back the previous layout right before the command.

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
