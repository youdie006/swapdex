// How the launcher reports the prebuilt binary's ending.
//
// `spawnSync` reports a signal death as `status: null` plus a signal name. The
// launcher used to collapse that to exit 1, which made "something on this
// machine killed the proxy" indistinguishable from "the proxy failed" - and a
// signal death prints nothing, so there was no other clue either. On WSL the
// proxy was dying at the top of every hour and systemd's restart hid it.
const { constants } = require("os");

function describeExit(result) {
  if (typeof result.status === "number") {
    return { code: result.status, note: null };
  }
  const signal = result.signal;
  if (!signal) {
    return {
      code: 1,
      note:
        "swapdex: the binary ended without an exit code and without a signal. " +
        "Exiting 1, but that 1 is this launcher's, not the binary's.",
    };
  }
  const number = constants.signals[signal];
  return {
    // 128 + signal is the shell convention, so 137 reads as SIGKILL rather
    // than as a swapdex failure.
    code: number ? 128 + number : 1,
    note:
      `swapdex: the binary was killed by ${signal}. Something on this machine ` +
      "sent that signal - this is not swapdex exiting on its own.",
  };
}

module.exports = { describeExit };
