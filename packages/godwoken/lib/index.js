const { Reader } = require("ckb-js-toolkit");
var addon = require("../native");

function buildGenesisBlock(config) {
  return JSON.parse(addon.buildGenesisBlock(JSON.stringify(config)));
}

class ChainService {
  constructor(config, headerInfo) {
    this.config = config;
    this.nativeChain = new addon.NativeChain(
      JSON.stringify(config),
      new Reader(headerInfo).toArrayBuffer()
    );
  }

  async sync(syncParam) {
    const syncEventString = this.nativeChain.sync(JSON.stringify(syncParam));
    return JSON.parse(syncEventString);
  }

  async produceBlock(produceBlockParam) {
    const produceBlockResult = this.nativeChain.produceBlock(
      JSON.stringify(produceBlockParam)
    );
    return JSON.parse(produceBlockResult);
  }
  async execute(l2Transaction) {
    const runResult = this.nativeChain.execute(
      new Reader(l2Transaction).toArrayBuffer()
    );
    return JSON.parse(runResult);
  }

  async submitL2Transaction(l2Transaction) {
    const runResult = this.nativeChain.submitL2Transaction(
      new Reader(l2Transaction).toArrayBuffer()
    );
    return JSON.parse(runResult);
  }

  async submitWithdrawalRequest(withdrawalRequest) {
    this.nativeChain.submitWithdrawalRequest(
      new Reader(withdrawalRequest).toArrayBuffer()
    );
  }

  async getBalance(accountId, sudtId) {
    return this.nativeChain.getBalance(accountId, sudtId);
  }

  async getStorageAt(accountId, rawKey) {
    return this.nativeChain.getStorageAt(
      accountId,
      new Reader(rawKey).toArrayBuffer()
    );
  }

  async getAccountIdByScriptHash(scriptHash) {
    return this.nativeChain.getAccountIdByScriptHash(
      new Reader(scriptHash).toArrayBuffer()
    );
  }

  async getNonce(accountId) {
    return this.nativeChain.getNonce(accountId);
  }

  async getScriptHash(accountId) {
    return this.nativeChain.getScriptHash(accountId);
  }

  async getScript(scriptHash) {
    const result = this.nativeChain.getScript(
      new Reader(scriptHash).toArrayBuffer()
    );
    if (result) {
      return JSON.parse(result);
    }
    return undefined;
  }

  async getDataHash(dataHash) {
    return this.nativeChain.getDataHash(new Reader(dataHash).toArrayBuffer());
  }

  async getData(dataHash) {
    return this.nativeChain.getData(new Reader(dataHash).toArrayBuffer());
  }

  tip() {
    return this.nativeChain.tip();
  }

  lastSynced() {
    return this.nativeChain.lastSynced();
  }

  status() {
    return this.nativeChain.status();
  }

  config() {
    return this.config;
  }
}

module.exports = { ChainService, buildGenesisBlock };
