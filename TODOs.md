current todos.

* in the todo means its a high priority, the more there is, the highest the chance it will be implemented on the next update
~ in the todo means its half-done/in-progress
x in the todo means its done
? in the todo means i cant recreate the issue
/ in the todo means its not too important
- in the todo means i couldnt fix it yet and will revisit later
$ in the todo means its an easy fix/feat

empty brackets means its untouched (also means i probably just dumped a random idea i had)

done todos get erased shortly after.

STUFF TO KEEP IN MIND:
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

issues:
[?/] - random not responding after exiting cs2 (might have to do with constant changes to resolution)
[?] - sometimes crashes after taking a windows screenshot (prntscreen button)
[*-] - opening youtube with ublock origin activated makes it open in a half-open half-not state.
[?] - saving a session with :w sometimes doesnt work, no idea why
[/?] - random not responding after new update 26/07/2026 22:54
[$**] - pressing a copy button using the hint mode doesnt actually copy it (tested on the copy repository button on github)
[***] - on the terminal, if i scroll up and start typing on claude code, the screen stays up there when its supposed to go down the second i start typing.
[*] - add a way for users to freely install extensions
[$/] - closing a split always makes the left-most split the highlighted one instead of the previously added split
[/] - fix :freeze, doesnt really do what its supposed to (its supposed to completely freeze everything thats using resources and drop the :resources to 30-60 mb, but it only reduces like 10%, so if the browser is using 1gb and i freeze everything it drops to 900mb instead of 30-60mb)

feats:
[] - make ";" toggle browser hud visibility
[***] - add inspect and view source to right mouse button, aswell as a command and a keyboard shortcut. 
[**] - make "shift+u" undo the layout, for example if i accidently close something, or move something around, U will undo it (maybe add R to redo it aswell) 
[$/] - maybe change ":q" to ":quit" or ":leave (:l)"? i keep quitting the browser accidently when i want to quit vim because i think im in passthrough when im in normal
[/] - add vertical sidebar like zen browser
[*] - make it so every browser related error is shown in :error/:errors (thinking of the future, where the shell's AI can fix the shells issues itself)
[$] - allow the easy addition of bangs via the AI like: "could you add a !osrs bang that searches the old school runescape wiki please?"

maybes:
[~] - allow :ai to completely customize the browser, for example "the commandbar is too small, make it 25% taller and change the background color to green" or "change X keybind to Y"
[**] - maybe add a :engine command to change browser engine? might be overkill and dont know if its possible
[/] - ctrl+: enters command bar in vim mode, allows vim motions.
[] - maybe change the :read command into a :html toggle like :js and :css? if you think about it, removing html is basically removing the dom parsing and rendering only text, which is exactly what :read does
