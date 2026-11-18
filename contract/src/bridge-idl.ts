// acrux_bridge's Anchor IDL, for building deposit and withdraw transactions with @coral-xyz/anchor:
// the same program on Solana and on Acrux. Copied from backend/chain/target/idl/acrux_bridge.json.
export const ACRUX_BRIDGE_IDL = {
 "address": "EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5",
 "metadata": {
  "name": "acrux_bridge",
  "version": "0.1.0",
  "spec": "0.1.0",
  "description": "Acrux bridge: SPL tokens from Solana onto Acrux and back"
 },
 "instructions": [
  {
   "name": "attest_cblock",
   "docs": [
    "Counts the votes this transaction's ed25519 instructions verified: each must be a validator",
    "of the cblock's epoch voting Notar or Finalize on its slot and hash."
   ],
   "discriminator": [
    249,
    31,
    41,
    162,
    158,
    119,
    73,
    175
   ],
   "accounts": [
    {
     "name": "cblock",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         99,
         98,
         108,
         111,
         99,
         107
        ]
       },
       {
        "kind": "account",
        "path": "cblock.hash",
        "account": "Cblock"
       }
      ]
     }
    },
    {
     "name": "set",
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         115,
         101,
         116
        ]
       },
       {
        "kind": "account",
        "path": "cblock.epoch",
        "account": "Cblock"
       }
      ]
     }
    },
    {
     "name": "instructions",
     "address": "Sysvar1nstructions1111111111111111111111111"
    }
   ],
   "args": []
  },
  {
   "name": "deposit",
   "docs": [
    "Locks `amount` of the depositor's tokens in the mint's vault and records the deposit for",
    "`recipient` on Acrux. The record holds what the vault received, after any Token-2022",
    "transfer fee."
   ],
   "discriminator": [
    242,
    35,
    198,
    137,
    82,
    225,
    242,
    182
   ],
   "accounts": [
    {
     "name": "depositor",
     "writable": true,
     "signer": true
    },
    {
     "name": "bridge",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         98,
         114,
         105,
         100,
         103,
         101
        ]
       }
      ]
     }
    },
    {
     "name": "mint"
    },
    {
     "name": "source",
     "writable": true
    },
    {
     "name": "vault",
     "docs": [
      "The mint's vault: a token account at `[\"vault\", mint]` that is its own authority, created",
      "by the mint's first deposit."
     ],
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         118,
         97,
         117,
         108,
         116
        ]
       },
       {
        "kind": "account",
        "path": "mint"
       }
      ]
     }
    },
    {
     "name": "record",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         100,
         101,
         112,
         111,
         115,
         105,
         116
        ]
       },
       {
        "kind": "account",
        "path": "bridge.deposits",
        "account": "Bridge"
       }
      ]
     }
    },
    {
     "name": "token_program"
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    },
    {
     "name": "event_authority",
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         95,
         95,
         101,
         118,
         101,
         110,
         116,
         95,
         97,
         117,
         116,
         104,
         111,
         114,
         105,
         116,
         121
        ]
       }
      ]
     }
    },
    {
     "name": "program"
    }
   ],
   "args": [
    {
     "name": "amount",
     "type": "u64"
    },
    {
     "name": "recipient",
     "type": "pubkey"
    }
   ]
  },
  {
   "name": "hand_off",
   "docs": [
    "Hands the light client on to `epoch`: the validator set a certified cblock of the epoch",
    "before carries in its outbox, proven against the outbox root."
   ],
   "discriminator": [
    17,
    228,
    210,
    156,
    26,
    220,
    158,
    101
   ],
   "accounts": [
    {
     "name": "payer",
     "writable": true,
     "signer": true
    },
    {
     "name": "cblock",
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         99,
         98,
         108,
         111,
         99,
         107
        ]
       },
       {
        "kind": "account",
        "path": "cblock.hash",
        "account": "Cblock"
       }
      ]
     }
    },
    {
     "name": "next",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         115,
         101,
         116
        ]
       },
       {
        "kind": "arg",
        "path": "epoch"
       }
      ]
     }
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    }
   ],
   "args": [
    {
     "name": "epoch",
     "type": "u64"
    },
    {
     "name": "validators",
     "type": {
      "vec": {
       "defined": {
        "name": "Validator"
       }
      }
     }
    },
    {
     "name": "proof",
     "type": {
      "vec": {
       "defined": {
        "name": "ProofNode"
       }
      }
     }
    }
   ]
  },
  {
   "name": "init_validator_set",
   "docs": [
    "Seeds the light client: Acrux's validators and stakes for `epoch`, in Acrux's validator order."
   ],
   "discriminator": [
    119,
    180,
    153,
    54,
    13,
    213,
    68,
    105
   ],
   "accounts": [
    {
     "name": "authority",
     "writable": true,
     "signer": true,
     "relations": [
      "bridge"
     ]
    },
    {
     "name": "bridge",
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         98,
         114,
         105,
         100,
         103,
         101
        ]
       }
      ]
     }
    },
    {
     "name": "set",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         115,
         101,
         116
        ]
       },
       {
        "kind": "arg",
        "path": "epoch"
       }
      ]
     }
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    }
   ],
   "args": [
    {
     "name": "epoch",
     "type": "u64"
    },
    {
     "name": "validators",
     "type": {
      "vec": {
       "defined": {
        "name": "Validator"
       }
      }
     }
    }
   ]
  },
  {
   "name": "initialize",
   "docs": [
    "Creates the bridge's one state account, which numbers the deposits. Its payer becomes the",
    "authority that seeds the first validator set; `epoch_slots` is an Acrux epoch's length."
   ],
   "discriminator": [
    175,
    175,
    109,
    31,
    13,
    152,
    155,
    237
   ],
   "accounts": [
    {
     "name": "payer",
     "writable": true,
     "signer": true
    },
    {
     "name": "bridge",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         98,
         114,
         105,
         100,
         103,
         101
        ]
       }
      ]
     }
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    }
   ],
   "args": [
    {
     "name": "epoch_slots",
     "type": "u64"
    }
   ]
  },
  {
   "name": "post_cblock",
   "docs": [
    "Posts a cblock to be certified, at `[\"cblock\", hash]`. The hash is recomputed here from the",
    "preimage, which starts with the cblock's slot, and the outbox root, so the root is bound to",
    "the hash the validators sign."
   ],
   "discriminator": [
    64,
    68,
    56,
    164,
    13,
    160,
    26,
    254
   ],
   "accounts": [
    {
     "name": "payer",
     "writable": true,
     "signer": true
    },
    {
     "name": "bridge",
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         98,
         114,
         105,
         100,
         103,
         101
        ]
       }
      ]
     }
    },
    {
     "name": "cblock",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         99,
         98,
         108,
         111,
         99,
         107
        ]
       },
       {
        "kind": "arg",
        "path": "hash"
       }
      ]
     }
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    }
   ],
   "args": [
    {
     "name": "hash",
     "type": {
      "array": [
       "u8",
       32
      ]
     }
    },
    {
     "name": "outbox_root",
     "type": {
      "array": [
       "u8",
       32
      ]
     }
    },
    {
     "name": "preimage",
     "type": "bytes"
    }
   ]
  },
  {
   "name": "release",
   "docs": [
    "On Solana: pays a withdrawal out of the mint's vault to `recipient`'s token account, proven",
    "against the outbox root of a certified cblock. Each sequence is paid once."
   ],
   "discriminator": [
    253,
    249,
    15,
    206,
    28,
    127,
    193,
    241
   ],
   "accounts": [
    {
     "name": "payer",
     "writable": true,
     "signer": true
    },
    {
     "name": "cblock",
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         99,
         98,
         108,
         111,
         99,
         107
        ]
       },
       {
        "kind": "account",
        "path": "cblock.hash",
        "account": "Cblock"
       }
      ]
     }
    },
    {
     "name": "mint"
    },
    {
     "name": "vault",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         118,
         97,
         117,
         108,
         116
        ]
       },
       {
        "kind": "account",
        "path": "mint"
       }
      ]
     }
    },
    {
     "name": "destination",
     "writable": true
    },
    {
     "name": "released",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         114,
         101,
         108,
         101,
         97,
         115,
         101,
         100
        ]
       },
       {
        "kind": "arg",
        "path": "sequence"
       }
      ]
     }
    },
    {
     "name": "token_program"
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    }
   ],
   "args": [
    {
     "name": "sequence",
     "type": "u64"
    },
    {
     "name": "recipient",
     "type": "pubkey"
    },
    {
     "name": "amount",
     "type": "u64"
    },
    {
     "name": "proof",
     "type": {
      "vec": {
       "defined": {
        "name": "ProofNode"
       }
      }
     }
    }
   ]
  },
  {
   "name": "withdraw",
   "docs": [
    "On Acrux: burns `amount` of a bridged token (the mint at `[\"mint\", solana_mint]`) and records",
    "the withdrawal to `recipient` on Solana at `[\"withdrawal\", sequence]`, for the cblock's outbox."
   ],
   "discriminator": [
    183,
    18,
    70,
    156,
    148,
    109,
    161,
    34
   ],
   "accounts": [
    {
     "name": "owner",
     "writable": true,
     "signer": true
    },
    {
     "name": "mint",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         109,
         105,
         110,
         116
        ]
       },
       {
        "kind": "arg",
        "path": "solana_mint"
       }
      ]
     }
    },
    {
     "name": "source",
     "writable": true
    },
    {
     "name": "outbound",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         111,
         117,
         116,
         98,
         111,
         117,
         110,
         100
        ]
       }
      ]
     }
    },
    {
     "name": "record",
     "writable": true,
     "pda": {
      "seeds": [
       {
        "kind": "const",
        "value": [
         119,
         105,
         116,
         104,
         100,
         114,
         97,
         119,
         97,
         108
        ]
       },
       {
        "kind": "account",
        "path": "outbound.withdrawals",
        "account": "Outbound"
       }
      ]
     }
    },
    {
     "name": "token_program"
    },
    {
     "name": "system_program",
     "address": "11111111111111111111111111111111"
    }
   ],
   "args": [
    {
     "name": "solana_mint",
     "type": "pubkey"
    },
    {
     "name": "amount",
     "type": "u64"
    },
    {
     "name": "recipient",
     "type": "pubkey"
    }
   ]
  }
 ],
 "accounts": [
  {
   "name": "Bridge",
   "discriminator": [
    231,
    232,
    31,
    98,
    110,
    3,
    23,
    59
   ]
  },
  {
   "name": "Cblock",
   "discriminator": [
    111,
    182,
    200,
    33,
    145,
    96,
    26,
    30
   ]
  },
  {
   "name": "DepositRecord",
   "discriminator": [
    83,
    232,
    10,
    31,
    251,
    49,
    189,
    167
   ]
  },
  {
   "name": "Outbound",
   "discriminator": [
    37,
    242,
    11,
    107,
    242,
    156,
    70,
    129
   ]
  },
  {
   "name": "Released",
   "discriminator": [
    200,
    119,
    122,
    5,
    75,
    119,
    141,
    241
   ]
  },
  {
   "name": "ValidatorSet",
   "discriminator": [
    35,
    206,
    97,
    202,
    43,
    11,
    11,
    127
   ]
  },
  {
   "name": "Withdrawal",
   "discriminator": [
    10,
    45,
    211,
    182,
    129,
    235,
    90,
    82
   ]
  }
 ],
 "events": [
  {
   "name": "Deposited",
   "discriminator": [
    111,
    141,
    26,
    45,
    161,
    35,
    100,
    57
   ]
  }
 ],
 "errors": [
  {
   "code": 6000,
   "name": "ZeroAmount",
   "msg": "A deposit moves more than nothing."
  },
  {
   "code": 6001,
   "name": "BadEpoch",
   "msg": "An epoch is a positive number of cblocks."
  },
  {
   "code": 6002,
   "name": "BadSet",
   "msg": "A validator set holds 1 to 32 validators."
  },
  {
   "code": 6003,
   "name": "BadPreimage",
   "msg": "The preimage and outbox root do not hash to the cblock's hash."
  },
  {
   "code": 6004,
   "name": "BadVote",
   "msg": "A signature is not a validator's Notar or Finalize vote on this cblock."
  },
  {
   "code": 6005,
   "name": "NotCertified",
   "msg": "The cblock is not certified in the epoch before the set it hands on."
  },
  {
   "code": 6006,
   "name": "BadProof",
   "msg": "The validator set is not in the cblock's outbox."
  }
 ],
 "types": [
  {
   "name": "Bridge",
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "deposits",
      "docs": [
       "Deposits made so far; the next deposit's sequence number."
      ],
      "type": "u64"
     },
     {
      "name": "bump",
      "type": "u8"
     },
     {
      "name": "authority",
      "docs": [
       "Seeds the first validator set."
      ],
      "type": "pubkey"
     },
     {
      "name": "epoch_slots",
      "docs": [
       "An Acrux epoch's length in slots."
      ],
      "type": "u64"
     }
    ]
   }
  },
  {
   "name": "Cblock",
   "docs": [
    "A cblock posted to the light client, at `[\"cblock\", hash]`, with the validators that voted for",
    "it as bits by their index in the set."
   ],
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "slot",
      "type": "u64"
     },
     {
      "name": "epoch",
      "type": "u64"
     },
     {
      "name": "hash",
      "type": {
       "array": [
        "u8",
        32
       ]
      }
     },
     {
      "name": "outbox_root",
      "type": {
       "array": [
        "u8",
        32
       ]
      }
     },
     {
      "name": "notar",
      "type": "u32"
     },
     {
      "name": "finalize",
      "type": "u32"
     },
     {
      "name": "certified",
      "type": "bool"
     }
    ]
   }
  },
  {
   "name": "DepositRecord",
   "docs": [
    "One deposit, at `[\"deposit\", sequence]`."
   ],
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "sequence",
      "type": "u64"
     },
     {
      "name": "depositor",
      "type": "pubkey"
     },
     {
      "name": "recipient",
      "docs": [
       "The Acrux address the deposit is credited to."
      ],
      "type": "pubkey"
     },
     {
      "name": "mint",
      "type": "pubkey"
     },
     {
      "name": "amount",
      "docs": [
       "What the vault received, in the mint's base units."
      ],
      "type": "u64"
     },
     {
      "name": "slot",
      "docs": [
       "The Solana slot the deposit landed in."
      ],
      "type": "u64"
     },
     {
      "name": "bump",
      "type": "u8"
     }
    ]
   }
  },
  {
   "name": "Deposited",
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "sequence",
      "type": "u64"
     },
     {
      "name": "depositor",
      "type": "pubkey"
     },
     {
      "name": "recipient",
      "type": "pubkey"
     },
     {
      "name": "mint",
      "type": "pubkey"
     },
     {
      "name": "amount",
      "type": "u64"
     },
     {
      "name": "slot",
      "type": "u64"
     }
    ]
   }
  },
  {
   "name": "Outbound",
   "docs": [
    "On Acrux: the withdrawals made so far, at `[\"outbound\"]`."
   ],
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "withdrawals",
      "type": "u64"
     },
     {
      "name": "bump",
      "type": "u8"
     }
    ]
   }
  },
  {
   "name": "ProofNode",
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "left",
      "docs": [
       "Whether the sibling sits left of the running hash."
      ],
      "type": "bool"
     },
     {
      "name": "hash",
      "type": {
       "array": [
        "u8",
        32
       ]
      }
     }
    ]
   }
  },
  {
   "name": "Released",
   "docs": [
    "On Solana: a withdrawal paid, at `[\"released\", sequence]`."
   ],
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "sequence",
      "type": "u64"
     },
     {
      "name": "cblock",
      "type": "pubkey"
     }
    ]
   }
  },
  {
   "name": "Validator",
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "key",
      "type": "pubkey"
     },
     {
      "name": "stake",
      "type": "u64"
     }
    ]
   }
  },
  {
   "name": "ValidatorSet",
   "docs": [
    "Acrux's validators and stakes for one epoch, at `[\"set\", epoch]`."
   ],
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "epoch",
      "type": "u64"
     },
     {
      "name": "total",
      "type": "u64"
     },
     {
      "name": "validators",
      "type": {
       "vec": {
        "defined": {
         "name": "Validator"
        }
       }
      }
     }
    ]
   }
  },
  {
   "name": "Withdrawal",
   "docs": [
    "On Acrux: one withdrawal, at `[\"withdrawal\", sequence]`."
   ],
   "type": {
    "kind": "struct",
    "fields": [
     {
      "name": "sequence",
      "type": "u64"
     },
     {
      "name": "owner",
      "type": "pubkey"
     },
     {
      "name": "recipient",
      "docs": [
       "The Solana address paid out of the vault."
      ],
      "type": "pubkey"
     },
     {
      "name": "solana_mint",
      "type": "pubkey"
     },
     {
      "name": "amount",
      "type": "u64"
     },
     {
      "name": "slot",
      "docs": [
       "The Acrux slot of the burn; its cblock's outbox carries the withdrawal."
      ],
      "type": "u64"
     },
     {
      "name": "bump",
      "type": "u8"
     }
    ]
   }
  }
 ]
} as const
