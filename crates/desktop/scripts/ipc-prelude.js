window.__post = window.__post || function (m) {
  try { if (window.ipc && /^https?:$/.test(location.protocol)) window.ipc.postMessage(m); } catch (e) {}
};
