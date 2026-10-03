// Servo transport only (0.5 and 0.6 have no native host-message callback). Shared shell scripts call __post as on WebView2.
// This runs in the page's realm: messages are untrusted, not privileged commands.
(function () {
  if (window.__servoQueueDocument === document) return;
  window.__servoQueueDocument = document;
  const owner = document;
  const stringify = JSON.stringify.bind(JSON);
  const id = String(Date.now()) + '-' + Math.random().toString(36).slice(2);
  let queue = [], size = 0, next = 1, dropped = 0;
  window.__post = function (text) {
    if (document !== owner || typeof text !== 'string') return;
    if (text.length > 8192 || queue.length >= 256 || size + text.length > 65536) {
      dropped++; return;
    }
    queue.push([next++, text]);
    size += text.length;
  };
  window.__servoShellRead = function (documentId, ack) {
    if (document !== owner) return 'null';
    // A read scheduled against the previous document must not acknowledge this one.
    if (documentId === id && Number.isSafeInteger(ack) && ack >= 0) {
      while (queue.length && queue[0][0] <= ack) size -= queue.shift()[1].length;
    }
    return stringify({ version: 1, document: id, dropped, messages: queue.slice(0, 32) });
  };
  // Keep the original input qualification lab usable until F6 enables shell mode.
  window.__mode = 'passthrough';
  window.__shellNativeContextMenu = true;
})();
