// client.ts
//TODO:  FIX  Uncaught SyntaxError: Failed to set the 'textContent' property on 'Node': Unexpected token 'export'
import * as anchor from "@project-serum/anchor";
import BN from "bn.js";
import {
  PublicKey,
  Keypair,
  SystemProgram,
  Transaction,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  getAssociatedTokenAddress,
  getAccount,
  createAssociatedTokenAccountInstruction,
} from "@solana/spl-token";

/**
 * Replace with your actual program ID (from declare_id! in lib.rs)
 * and, if available, the IDL JSON (Anchor build artifact).
 */
const PROGRAM_ID = new PublicKey("ebvdBEBKz6UK1Xs9mnGs7TsR2vgKyPP2idaFEqGRTRQ");
const idl: any = {}; // <-- paste your IDL JSON here if available

// Playground/Anchor globals (when running inside Playground tests or direct Anchor tests)
declare const pg: any | undefined;
declare const program: any | undefined;

/* ---------------------------- Runtime detection --------------------------- */

function providerCtor() {
  // Anchor might expose AnchorProvider or Provider depending on version
  return (anchor as any).AnchorProvider ?? (anchor as any).Provider ?? (anchor as any).Provider;
}

function walletCtor() {
  return (anchor as any).Wallet ?? (anchor as any).NodeWallet ?? null;
}

/** Create an Anchor Provider-like object bound to a Keypair payer (for use when runtime doesn't expose a payer). */
function makeProvider(connection: any, payerKeypair: Keypair) {
  const WalletCtor: any = walletCtor();
  let wallet: any;
  if (WalletCtor) {
    try {
      wallet = new WalletCtor(payerKeypair);
    } catch {
      wallet = (WalletCtor as any)(payerKeypair);
    }
  } else {
    // minimal wallet wrapper
    wallet = {
      publicKey: payerKeypair.publicKey,
      payer: payerKeypair,
      signTransaction: async (tx: Transaction) => {
        tx.partialSign(payerKeypair);
        return tx;
      },
      signAllTransactions: async (txs: Transaction[]) => {
        for (const tx of txs) tx.partialSign(payerKeypair);
        return txs;
      },
    };
  }

  const ProviderCtor: any = providerCtor();
  const provider = new ProviderCtor(connection, wallet, ProviderCtor.defaultOptions?.() ?? {});
  // store internal payer for fallbacks
  (provider as any).__internalPayer = payerKeypair;
  return provider;
}

/** Resolve runtime environment. Prefer Playground `pg`, else Anchor `program`. */
async function resolveRuntime() {
  // Playground path
  if (typeof pg !== "undefined" && pg != null) {
    const conn = pg.connection;
    const pgProgram = pg.program;
    const pgWallet = pg.wallet ?? null;

    if (!conn) throw new Error("Playground found but pg.connection is missing");
    if (!pgProgram) throw new Error("Playground found but pg.program is missing");

    // try to find a signer Keypair on the playground wallet or program provider
    let payerKeypair: Keypair | null = null;
    if (pgWallet && (pgWallet as any).payer) {
      payerKeypair = (pgWallet as any).payer as Keypair;
    } else if (pgProgram.provider && pgProgram.provider.wallet && (pgProgram.provider.wallet as any).payer) {
      payerKeypair = (pgProgram.provider.wallet as any).payer as Keypair;
    }

    if (!payerKeypair) {
      // create a temporary payer and request a small airdrop (best-effort)
      console.warn("Playground wallet found but no payer Keypair: creating temporary payer and requesting airdrop.");
      payerKeypair = Keypair.generate();
      try {
        const sig = await conn.requestAirdrop(payerKeypair.publicKey, LAMPORTS_PER_SOL);
        await conn.confirmTransaction(sig, "confirmed");
      } catch (e) {
        console.warn("Airdrop may fail in some environments (rate-limited):", e);
      }
    }

    // If the playground program exposes an IDL, construct an Anchor Program using our provider to get typed builders
    if (pgProgram.idl) {
      const provider = makeProvider(conn, payerKeypair);
      const anchorProg = new (anchor as any).Program(pgProgram.idl, pgProgram.programId ?? PROGRAM_ID, provider);
      return { program: anchorProg, connection: conn, wallet: provider.wallet, payerKeypair, provider };
    }

    // fallback: return playground program client and a created provider for signing utilities
    const provider = makeProvider(conn, payerKeypair);
    return { program: pgProgram, connection: conn, wallet: provider.wallet, payerKeypair, provider };
  }

  // Anchor `program` path
  if (typeof program !== "undefined" && program != null) {
    const prov = (program as any).provider;
    if (!prov) throw new Error("Found `program` but program.provider missing");
    const conn = prov.connection;
    const w = prov.wallet;

    let payerKeypair: Keypair | null = null;
    if ((w as any).payer) payerKeypair = (w as any).payer as Keypair;

    if (!payerKeypair) {
      console.warn("Anchor provider wallet has no payer — creating temporary payer and requesting airdrop.");
      payerKeypair = Keypair.generate();
      try {
        const sig = await conn.requestAirdrop(payerKeypair.publicKey, LAMPORTS_PER_SOL);
        await conn.confirmTransaction(sig, "confirmed");
      } catch (e) {
        console.warn("Airdrop may fail in some environments:", e);
      }
      const provider = makeProvider(conn, payerKeypair);
      const anchorProg = new (anchor as any).Program((program as any).idl ?? idl, program.programId ?? PROGRAM_ID, provider);
      return { program: anchorProg, connection: conn, wallet: provider.wallet, payerKeypair, provider };
    }

    return { program, connection: conn, wallet: w, payerKeypair, provider: prov };
  }

  throw new Error("Cannot detect runtime environment: expected Playground `pg` or Anchor `program`");
}

/* ---------------------------- PDA helpers ------------------------------- */

/** Seeds used by your Rust: ["v3","pool", mint_a, mint_b] */
async function pdaPool(mintA: PublicKey, mintB: PublicKey, progId: PublicKey) {
  return await PublicKey.findProgramAddress(
    [Buffer.from("v3"), Buffer.from("pool"), mintA.toBuffer(), mintB.toBuffer()],
    progId
  );
}
async function pdaVault(pool: PublicKey, mint: PublicKey, progId: PublicKey) {
  return await PublicKey.findProgramAddress(
    [Buffer.from("v3"), Buffer.from("vault"), pool.toBuffer(), mint.toBuffer()],
    progId
  );
}
async function pdaTreasury(pool: PublicKey, mint: PublicKey, progId: PublicKey) {
  return await PublicKey.findProgramAddress(
    [Buffer.from("v3"), Buffer.from("treasury"), pool.toBuffer(), mint.toBuffer()],
    progId
  );
}
async function pdaOrderbook(pool: PublicKey, progId: PublicKey) {
  return await PublicKey.findProgramAddress(
    [Buffer.from("v3"), Buffer.from("orderbook"), pool.toBuffer()],
    progId
  );
}
function receiptNonceBuf(nonce: number) {
  return new BN(nonce).toArrayLike(Buffer, "le", 8);
}
async function pdaPosition(pool: PublicKey, owner: PublicKey, receiptNonce: number, progId: PublicKey) {
  const nb = receiptNonceBuf(receiptNonce);
  return await PublicKey.findProgramAddress(
    [Buffer.from("v3"), Buffer.from("pos"), pool.toBuffer(), owner.toBuffer(), nb],
    progId
  );
}

/* ---------------------------- ATA helper ------------------------------- */

async function getOrCreateATA(connection: any, payerKeypair: Keypair, mint: PublicKey, owner: PublicKey, provider: any) {
  const ata = await getAssociatedTokenAddress(mint, owner);
  try {
    await getAccount(connection, ata);
    return ata;
  } catch {
    const ix = createAssociatedTokenAccountInstruction(payerKeypair.publicKey, ata, owner, mint);
    const tx = new Transaction().add(ix);
    // use provider if available
    if (provider?.sendAndConfirm) {
      await provider.sendAndConfirm(tx, []);
    } else if (provider?.provider?.sendAndConfirm) {
      await provider.provider.sendAndConfirm(tx, []);
    } else {
      // fallback to connection.sendTransaction with payer signing
      tx.feePayer = payerKeypair.publicKey;
      tx.recentBlockhash = (await (provider?.connection ?? provider?.rpc ?? provider?.connection).getRecentBlockhash()).blockhash;
      tx.partialSign(payerKeypair);
      const raw = tx.serialize();
      const sig = await (provider?.connection ?? provider?.rpc ?? provider?.connection).sendRawTransaction(raw);
      await (provider?.connection ?? provider?.rpc ?? provider?.connection).confirmTransaction(sig, "confirmed");
    }
    return ata;
  }
}

/* ------------------------------ RPC callers ----------------------------- */

/**
 * Try to call program methods using both camelCase and snake_case RPC names and account name variants.
 * - methodCandidates: e.g. ["initializePool","initialize_pool"]
 * - arg: the instruction struct or args
 * - accountVariants: array of account maps to try
 */
async function tryMethodWithAccountVariants(programClient: any, methodCandidates: string[], arg: any, accountVariants: Array<Record<string, any>>) {
  let lastErr: any = null;
  for (const m of methodCandidates) {
    // try new builder style
    const builder = programClient.methods?.[m];
    if (builder) {
      for (const accounts of accountVariants) {
        try {
          const call = builder(arg);
          const rpc = await call.accounts(accounts).rpc();
          return rpc;
        } catch (e) {
          lastErr = e;
        }
      }
    } else {
      // try older rpc style e.g. program.rpc.initialize_pool(...)
      const rpcFn = programClient.rpc?.[m];
      if (rpcFn) {
        for (const accounts of accountVariants) {
          try {
            // older style may accept arg as single param or spread; try single param pattern
            const res = await rpcFn(arg, { accounts });
            return res;
          } catch (e) {
            lastErr = e;
          }
        }
      }
    }
  }
  throw lastErr ?? new Error("No matching RPC method found");
}

/* ---------------------------- High-level API ---------------------------- */

/** Initialize pool */
export async function initializePool(mintA: PublicKey, mintB: PublicKey, params: any) {
  const env = await resolveRuntime();
  const progClient = env.program;
  const progId: PublicKey = (progClient as any).programId ?? PROGRAM_ID;

  const [poolPda] = await pdaPool(mintA, mintB, progId);
  const [vaultAPda] = await pdaVault(poolPda, mintA, progId);
  const [vaultBPda] = await pdaVault(poolPda, mintB, progId);
  const [treasuryAPda] = await pdaTreasury(poolPda, mintA, progId);
  const [treasuryBPda] = await pdaTreasury(poolPda, mintB, progId);

  const payerPub = env.payerKeypair.publicKey;

  // account variants (snake_case & camelCase)
  const accountVariants = [
    {
      payer: payerPub,
      mint_a: mintA,
      mint_b: mintB,
      pool: poolPda,
      vault_a: vaultAPda,
      vault_b: vaultBPda,
      treasury_a: treasuryAPda,
      treasury_b: treasuryBPda,
      token_program: TOKEN_PROGRAM_ID,
      system_program: SystemProgram.programId,
    },
    {
      payer: payerPub,
      mintA: mintA,
      mintB: mintB,
      pool: poolPda,
      vaultA: vaultAPda,
      vaultB: vaultBPda,
      treasuryA: treasuryAPda,
      treasuryB: treasuryBPda,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    },
  ];

  try {
    const sig = await tryMethodWithAccountVariants(progClient, ["initializePool", "initialize_pool"], params, accountVariants);
    await env.connection.confirmTransaction(sig, "confirmed");
    return sig;
  } catch (err) {
    console.error("initializePool failed:", err);
    throw err;
  }
}

/** post_yields_and_update
 *  yA/yB: number (u16) or BN-like for compatibility
 *  spotPrice: BN or number for u64
 *  cuPrice: BN or number as u64
 */
export async function postYieldsAndUpdate(yA: number | BN, yB: number | BN, spotPrice: BN | number, cuPrice: BN | number, opts?: { caller?: PublicKey, metrics?: PublicKey | null }) {
  const env = await resolveRuntime();
  const progClient = env.program;
  const progId: PublicKey = (progClient as any).programId ?? PROGRAM_ID;
  const poolPda = opts?.caller ? null : null; // not used here; caller provided in accounts
  const payerPub = env.payerKeypair.publicKey;

  // default caller is payer
  const caller = opts?.caller ?? payerPub;

  // Build account variants
  const accountVariants = [
    {
      caller,
      oracle_signer_opt: null,
      pool: opts?.caller /* noop */ ?? undefined, // placeholder; we will set real pool at call-time by user
      treasury_a: undefined,
      treasury_b: undefined,
      caller_ata_a: undefined,
      caller_ata_b: undefined,
      mint_a: undefined,
      mint_b: undefined,
      vault_a: undefined,
      vault_b: undefined,
      metrics: opts?.metrics ?? null,
      token_program: TOKEN_PROGRAM_ID,
    },
  ];

  // This function needs the caller to replace undefineds with real accounts before calling.
  // For convenience, we attempt to call both method name variants and assume caller passes exact accounts.
  const methodCandidates = ["postYieldsAndUpdate", "post_yields_and_update"];

  // The caller should call this helper by passing the correct accounts through 'progClient.methods(...)' normally.
  // We'll attempt builder call if available, but we can't construct all accounts generically here.
  if ((progClient as any).methods?.postYieldsAndUpdate || (progClient as any).methods?.post_yields_and_update) {
    const builderName = (progClient as any).methods?.postYieldsAndUpdate ? "postYieldsAndUpdate" : "post_yields_and_update";
    const call = (progClient as any).methods[builderName](typeof yA === "number" ? yA : yA, typeof yB === "number" ? yB : yB, spotPrice, cuPrice);
    return { builder: call, env };
  } else {
    throw new Error("Program client missing post_yields_and_update builder. Use the Anchor program client directly with accounts.");
  }
}

/** addLiquidity helper uses the program builder and returns the tx sig */
export async function addLiquidity(
  mintA: PublicKey,
  mintB: PublicKey,
  bandIdx: number,
  amountA: BN,
  amountB: BN,
  receiptNonce: number,
  minUnlockAfterSlots: BN = new BN(0)
) {
  const env = await resolveRuntime();
  const progClient = env.program;
  const progId: PublicKey = (progClient as any).programId ?? PROGRAM_ID;
  const [poolPda] = await pdaPool(mintA, mintB, progId);
  const [vaultA] = await pdaVault(poolPda, mintA, progId);
  const [vaultB] = await pdaVault(poolPda, mintB, progId);

  const payer = env.payerKeypair;
  const payerPub = payer.publicKey;

  // ensure ATAs exist for payer (caller)
  const userAtaA = await getOrCreateATA(env.connection, payer, mintA, payerPub, env.provider);
  const userAtaB = await getOrCreateATA(env.connection, payer, mintB, payerPub, env.provider);

  const [posPda] = await pdaPosition(poolPda, payerPub, receiptNonce, progId);

  // Try both method name variants
  const builder = (progClient as any).methods?.addLiquidity ?? (progClient as any).methods?.add_liquidity;
  if (!builder) throw new Error("Program client missing addLiquidity builder (check IDL).");

  try {
    const rpc = await builder(bandIdx, amountA, amountB, new BN(receiptNonce), minUnlockAfterSlots)
      .accounts({
        user: payerPub,
        pool: poolPda,
        vault_a: vaultA,
        vault_b: vaultB,
        user_ata_a: userAtaA,
        user_ata_b: userAtaB,
        position: posPda,
        token_program: TOKEN_PROGRAM_ID,
        system_program: SystemProgram.programId,
        mint_a: mintA,
        mint_b: mintB,
      })
      .rpc();
    await env.connection.confirmTransaction(rpc, "confirmed");
    return rpc;
  } catch (err) {
    console.error("addLiquidity failed:", err);
    throw err;
  }
}

/** placeOrder helper (simplified) */
export async function placeOrder(
  mintA: PublicKey,
  mintB: PublicKey,
  sideObj: any, // { Bid: {} } or { Ask: {} }
  qty: BN,
  limitPriceOpt: BN | null,
  tifParam: any,
  postOnly = false,
  reduceOnly = false,
  clientId: BN = new BN(0)
) {
  const env = await resolveRuntime();
  const progClient = env.program;
  const payerPub = env.payerKeypair.publicKey;
  const progId: PublicKey = (progClient as any).programId ?? PROGRAM_ID;
  const [poolPda] = await pdaPool(mintA, mintB, progId);
  const [orderbookPda] = await pdaOrderbook(poolPda, progId);

  const builder = (progClient as any).methods?.placeOrder ?? (progClient as any).methods?.place_order;
  if (!builder) throw new Error("Program client missing placeOrder builder (check IDL).");

  try {
    const rpc = await builder(sideObj, qty, limitPriceOpt, tifParam, postOnly, reduceOnly, clientId)
      .accounts({
        user: env.payerKeypair.publicKey,
        pool: poolPda,
        orderbook: orderbookPda,
      })
      .rpc();
    await env.connection.confirmTransaction(rpc, "confirmed");
    return rpc;
  } catch (err) {
    console.error("placeOrder failed:", err);
    throw err;
  }
}

/* Utility functions */

export async function viewPoolState(mintA: PublicKey, mintB: PublicKey) {
  const env = await resolveRuntime();
  const progClient = env.program;
  const progId: PublicKey = (progClient as any).programId ?? PROGRAM_ID;
  const [poolPda] = await pdaPool(mintA, mintB, progId);

  if ((progClient as any).account?.pool) {
    const acct = await (progClient as any).account.pool.fetch(poolPda);
    return acct;
  } else {
    console.warn("Program client doesn't have typed account access to pool; returning PDA only.");
    return { pda: poolPda };
  }
}

export async function showMyAddressAndBalance() {
  const env = await resolveRuntime();
  const bal = await env.connection.getBalance(env.payerKeypair.publicKey);
  console.log("Address:", env.payerKeypair.publicKey.toBase58(), "Balance:", bal / LAMPORTS_PER_SOL);
}

/* ==================== Attach to Playground (if present) =================== */

(async () => {
  try {
    const env = await resolveRuntime();
    if (typeof pg !== "undefined" && pg != null) {
      // attach a friendly API for interactive console use in Playground
      (pg as any).yieldSensitiveDlmm = {
        initializePool,
        postYieldsAndUpdate,
        addLiquidity,
        placeOrder,
        viewPoolState,
        showMyAddressAndBalance,
      };
      console.log("Attached yieldSensitiveDlmm API to pg.yieldSensitiveDlmm");
    }
  } catch (e) {
    // ignore: not running inside Playground
  }
})();

export default {
  initializePool,
  postYieldsAndUpdate,
  addLiquidity,
  placeOrder,
  viewPoolState,
  showMyAddressAndBalance,
};
