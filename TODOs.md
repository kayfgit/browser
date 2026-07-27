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
[*] - if i zoom on a tab it changes the zoom for all the tabs, both the browser and content zoom. it should be individual (since browser zoom is related to terminal zoom, separating browser and terminal might be needed)
[*] - :ads should only toggle the current adblocker, if i have native on and run :ads it will turn it off as intended, but running :ads again puts me in ubo and not native
[*] - insert mode should also be exitable with ctrl+s or shift+escape
[] - clicking with the mouse through the splits sometimes doesnt change cleanly, if i have chatgpt on the left and a terminal on the right and the current selected is chatgpt and the split is in insert mode, if i click on the terminal with the mouse the blue border highlights shift to the terminal, but the chatgpt tab continues the active one, so typing will go to the chatgpt tab
[] - still has that annoying issue of the commandbar being frozen but only sometimes
[*] - caret mode cursor is not placed correctly when selecting, its always one to the right. the word "apple" for example, if the cursor is on the "a" of the "apple" and i start selecting, the selection doesnt happen until i move the cursor somewhere (it should, just like vim), so if i select the word until the cursor is on "e" of the "apple" and i yank it, i only yank "appl" and not "apple"
[] - saving a session with :w sometimes doesnt work, no idea why

feats:
[*] - :save/saved or :favorite/favorites for saving a page for later
[*] - add copy to right mouse button click
[] - make ";" toggle browser hud visibility

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
