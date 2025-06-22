import("./pkg")
  .then((rust_module) => {
    console.log("Wasm module loaded");
    let handle = null;
    const playback_button = document.getElementById("playback");
    playback_button.addEventListener("click", (event) => {
      console.log("Playback button clicked");
      handle = rust_module.playback();
    });

    const record_button = document.getElementById("record");
    record_button.addEventListener("click", (event) => {
      console.log("Record button clicked");
      handle = rust_module.record();
    });

    const beep_button = document.getElementById("beep");
    beep_button.addEventListener("click", (event) => {
      console.log("Beep button clicked");
      handle = rust_module.beep();
    });

    const stop_button = document.getElementById("stop");
    stop_button.addEventListener("click", (event) => {
      console.log("Stop button clicked");
      if (handle != null) {
        handle.free();
        handle = null;
      }
    });
  })
  .catch(console.error);
