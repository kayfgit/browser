current todos.

* in the todo means its a high priority, the more there is, the highest the chance it will be implemented on the next update
~ in the todo means its half-done/in-progress
x in the todo means its done (or if it just disappears)
? in the todo means i cant recreate the issue
/ in the todo means its not too important
- in the todo means i couldnt fix it yet and will revisit later
$ in the todo means its an easy fix/feat
empty brackets means its untouched (also means i probably just dumped a random idea/issue i had)

done todos get erased shortly after.

STUFF TO KEEP IN MIND:
- because i made this for myself first, its only ported to windows which is my daily driver. if someone wants ill port it to lsomeone wants ill port it to linux/mac aswell
- i dont really care about looks (will probably be one of the last things i care about)
- i dont really care about security *as of now* (of course i care about it, but i have to make the browser actually usable before making it secure)
- i REALLY care about efficiency
- i REALLY care about resource usage
- i REALLY care about speed
- i REALLY care about how lightweight it is
- I REALLY CARE ABOUT MALLEABILITY, users should be able to do absolutely anything they want with it

general:
[*] - ship every engine with the browser by default: the installer has all engines checked and users can uncheck the ones they dont want. package each engine separately so updates only redownload the browser (~20mb) unless an engine changed, and allow adding/removing engines later (something like :engine install gecko / :engine remove servo)
[] - maybe freeze the adblock feature for now, it doesnt really work as intended (ill probably have to rework it)
[] - add a demo video on the readme

issues:
[?/] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[*-] - opening youtube with ublock origin activated makes it open in a half-open half-not state.
[?] - saving a session with :w sometimes doesnt work, no idea why
[/?] - random not responding after new update 26/07/2026 22:54
[*] - add a way for users to freely install extensions
[] - fix the skip ad button not being pressable with hint mode
[] - :resources and every other native shell command should not override the current pane/tab, currently if i have a splitted webview2/servo open and write :res it opens on the selected pane

feats:
[] - make ";" toggle browser hud visibility
[/] - add vertical sidebar like zen browser
[*] - make it so every browser related error is shown in :error/:errors (like if an engine crashes/errors, extension stops working, everything)
[] - allow splitting without having anything open (like on the welcome screen), basically treat the welcome screen as an empty pane that cannot be closed if its by itself, but closeable if theres other splits, if the user closes every split then they go back to the welcome screen

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[~*] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
[] - maybe change the :read command into a :html toggle like :js and :css? if you think about it, removing html is basically removing the dom parsing and rendering only text, which is exactly what :read does
[] - maybe add the zen browser glance feature, where i can preview a link in a floating window
