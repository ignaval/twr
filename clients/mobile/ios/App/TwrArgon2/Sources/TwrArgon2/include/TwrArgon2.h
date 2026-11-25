#ifndef TWR_ARGON2_H
#define TWR_ARGON2_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Argon2id, version 0x13, raw output: the stretching step of the phone-pairing proof key
 * (docs/design/phone-pairing.md). `m_kib` is the memory cost in KiB, `t_cost` the passes,
 * `lanes` the parallelism (computed on one thread). Returns 0 on success, or the reference
 * implementation's negative ARGON2_* error code. Blocks for as long as the hash takes.
 */
int twr_argon2id(const void *password, size_t password_len,
                 const void *salt, size_t salt_len,
                 uint32_t t_cost, uint32_t m_kib, uint32_t lanes,
                 void *out, size_t out_len);

#ifdef __cplusplus
}
#endif

#endif
