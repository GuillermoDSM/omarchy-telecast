const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const model = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../MiracastModel.js"), "utf8").replace(/^\.pragma library\s*/, ""), model);

assert.equal(model.phaseLabel("p2p"), "WI-FI DIRECT");
assert.equal(model.phaseLabel("pairing"), "EMPAREJANDO");
assert.equal(model.phaseLabel("unknown"), "ENVIAR PANTALLA");
assert.equal(model.protocolLabel("airplay"), "AIRPLAY");
assert.equal(model.protocolLabel("miracast"), "MIRACAST");
assert.equal(model.bitrate(5000), "5.00 Mb/s");
assert.equal(model.bitrate(NaN), "—");
assert.equal(model.plain("TV\n\x1b"), "TV  ");
assert.equal(model.peers(null).length, 0);
const peers = model.peers([
  { mac: "be:7e:8b:be:29:2e", name: "Samsung\nTV", wfd: true, strength: 200 },
  { mac: "BE:7E:8B:BE:29:2E", name: "Duplicate", wfd: true },
  { mac: "anything", wfd: true },
  { mac: "00:00:00:00:00:01", wfd: false },
  { protocol: "airplay", mac: "68:72:C3:A8:40:DF", name: "Samsung AirPlay", model: "UTU7000", ip: "192.168.1.139", port: 59757 },
  { protocol: "airplay", mac: "00:00:00:00:00:02", name: "sin IP", ip: "", port: 59757 }
]);
assert.equal(peers.length, 2);
assert.equal(peers[0].protocol, "airplay");
assert.equal(peers[0].key, "airplay:68:72:C3:A8:40:DF");
assert.equal(peers[0].port, 59757);
assert.equal(peers[1].mac, "BE:7E:8B:BE:29:2E");
assert.equal(peers[1].name, "Samsung TV");
assert.equal(peers[1].strength, 100);
assert.equal(peers[1].protocol, "miracast");
assert.equal(model.connectRequest(peers[0], false).display_mode, undefined);
assert.equal(model.connectRequest(peers[1], true).display_mode, undefined);
assert.equal(model.connectRequest(peers[1], true).test, true);
assert.equal(model.connectRequest(peers[1], true).mac, peers[1].mac);
console.log("Model: all tests passed");
