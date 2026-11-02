const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const model = vm.createContext({});
vm.runInContext(fs.readFileSync(path.join(__dirname, "../MiracastModel.js"), "utf8").replace(/^\.pragma library\s*/, ""), model);

assert.equal(model.phaseLabel("p2p"), "WI-FI DIRECT");
assert.equal(model.phaseLabel("unknown"), "MIRACAST");
assert.equal(model.bitrate(5000), "5.00 Mb/s");
assert.equal(model.bitrate(NaN), "—");
assert.equal(model.plain("TV\n\x1b"), "TV  ");
assert.equal(model.peers(null).length, 0);
const peers = model.peers([
  { mac: "be:7e:8b:be:29:2e", name: "Samsung\nTV", wfd: true, strength: 200 },
  { mac: "BE:7E:8B:BE:29:2E", name: "Duplicate", wfd: true },
  { mac: "anything", wfd: true },
  { mac: "00:00:00:00:00:01", wfd: false }
]);
assert.equal(peers.length, 1);
assert.equal(peers[0].mac, "BE:7E:8B:BE:29:2E");
assert.equal(peers[0].name, "Samsung TV");
assert.equal(peers[0].strength, 100);
console.log("Model: all tests passed");
