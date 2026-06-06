[] - :shell should see if the given shell actually exists before applying
[] - command bar freezes while loading content
[] - autocomplete based on history, if i have accessed youtube.com recently and write ":open yout-" it should show the auto complete, which i can press tab to accept the autocomplete.
[] - look for a way to make the browser consume the least amount of resources from the pc, even with the heavy engine running
[x] - issue when using ":open", it says "failed to open: WebView2 error: WindowsError(Error { code: HRESULT(0x8007139f), message: 'O grup})" and i cant read the rest because its all inline. (root cause: WebView2 needs identical browser args across all webviews sharing the user-data folder, else the 2nd one fails with 0x8007139F/ERROR_INVALID_STATE — the terminal webview had no args while content tabs did. fixed by a shared BROWSER_ARGS const on every webview. for reading long errors: :error / :errors opens them in a scrollable vim pager.)
[] - maybe add tmux functionality? could be cool
[] - maybe add a :engine command to change browser engine? might be overkill
