const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const model = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../MiracastModel.js"), "utf8").replace(/^\.pragma library\s*/, ""), model);

assert.equal(model.phaseLabel("capture"), "ELIGE PANTALLA");
assert.equal(model.phaseLabel("pairing"), "EMPAREJANDO");
assert.equal(model.phaseLabel("unknown"), "ENVIAR PANTALLA");
assert.equal(model.plain("TV\n\x1b"), "TV  ");
assert.equal(model.peers(null).length, 0);
const peers = model.peers([
  { mac: "68:72:C3:A8:40:DF", name: "Samsung\nTV", model: "UTU7000", ip: "192.168.1.139", port: 59757 },
  { mac: "68:72:C3:A8:40:DF", name: "Duplicate", ip: "192.168.1.139", port: 59757 },
  { name: "sin IP", ip: "", port: 59757 },
  { mac: "00:00:00:00:00:02", name: "sin puerto", ip: "192.168.1.2", port: 0 }
]);
assert.equal(peers.length, 1);
assert.equal(peers[0].key, "airplay:68:72:C3:A8:40:DF");
assert.equal(peers[0].name, "Samsung TV");
assert.equal(peers[0].port, 59757);
const request = model.connectRequest(peers[0], true);
assert.equal(request.protocol, undefined);
assert.equal(request.test, true);
assert.equal(request.mac, peers[0].mac);
assert.equal(request.port, 59757);
console.log("Model: all tests passed");
