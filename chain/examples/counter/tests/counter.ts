import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { assert } from "chai";
import { Counter } from "../target/types/counter";

describe("counter", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.counter as Program<Counter>;
  const counter = anchor.web3.Keypair.generate();

  it("creates a counter at zero", async () => {
    await program.methods.initialize().accounts({ counter: counter.publicKey }).signers([counter]).rpc();
    const account = await program.account.counter.fetch(counter.publicKey);
    assert.equal(account.count.toNumber(), 0);
    assert.ok(account.authority.equals(provider.wallet.publicKey));
  });

  it("increments it three times", async () => {
    for (let i = 0; i < 3; i++) await program.methods.increment().accounts({ counter: counter.publicKey }).rpc();
    const account = await program.account.counter.fetch(counter.publicKey);
    assert.equal(account.count.toNumber(), 3);
  });

  it("refuses an increment from another authority", async () => {
    const other = anchor.web3.Keypair.generate();
    try {
      await program.methods.increment().accounts({ counter: counter.publicKey, authority: other.publicKey }).signers([other]).rpc();
      assert.fail("the increment went through");
    } catch (error) {
      assert.include(String(error), "has one constraint was violated");
    }
  });
});
