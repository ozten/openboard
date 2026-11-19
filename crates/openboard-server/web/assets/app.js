// Placeholder for the Openboard web UI. The real script replaces this file.
"use strict";

(function () {
  var status = document.getElementById("status");
  if (!status) {
    return;
  }
  fetch("/v1/health", { cache: "no-store" })
    .then(function (response) {
      return response.json();
    })
    .then(function (health) {
      status.textContent =
        "This board is running (" + health.mode + " mode, version " + health.version +
        "). The web UI is not built yet.";
    })
    .catch(function () {
      status.textContent = "The board could not be reached.";
    });
})();
