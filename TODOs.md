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
[*] - on the installer, allow users to choose which engine to get first, if they want to change engines or want multiple engines they can download it somehow later
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

feats:
[] - make ";" toggle browser hud visibility
[***] - add inspect and view source to right mouse button, aswell as a command and a keyboard shortcut. 
[**] - make "shift+u" undo the layout, for example if i accidently close something, or move something around, U will undo it (maybe add R to redo it aswell) 
[$/] - maybe change ":q" to ":quit" or ":leave (:l)"? i keep quitting the browser accidently when i want to quit vim because i think im in passthrough when im in normal
[/] - add vertical sidebar like zen browser
[*] - make it so every browser related error is shown in :error/:errors (thinking of the future, where the shell's AI can fix the shells issues itself)
[$] - allow the easy addition of bangs via the AI like: "could you add a !osrs bang that searches the old school runescape wiki please?"
[] - add :update that will fetch the newest release (because i kind of keep updating everyday, so downloading a new version everytime might get annoying)

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[**] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
[] - maybe change the :read command into a :html toggle like :js and :css? if you think about it, removing html is basically removing the dom parsing and rendering only text, which is exactly what :read does
