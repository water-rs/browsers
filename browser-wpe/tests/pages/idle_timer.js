// Reports from a background timer that mutates the document.
//
// Nothing here is reachable from an input event, an animation frame or a fetch:
// a `setTimeout` is the only thing that starts it, its callback writes to the
// DOM, and it reads that write back before reporting it. So a report arriving at
// all is the engine's main loop having been iterated while the page was idle,
// and the value proves the mutation was applied rather than merely queued.
addEventListener("DOMContentLoaded", () => {
  setTimeout(() => {
    document.body.dataset.idleTimer = "fired";
    waterui.invoke("report", {
      idleTimer: document.body.dataset.idleTimer,
    });
  }, 250);
});
