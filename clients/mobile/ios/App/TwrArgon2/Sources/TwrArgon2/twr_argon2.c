#include "TwrArgon2.h"
#include "argon2.h"

int twr_argon2id(const void *password, size_t password_len,
                 const void *salt, size_t salt_len,
                 uint32_t t_cost, uint32_t m_kib, uint32_t lanes,
                 void *out, size_t out_len) {
    if (password == NULL || salt == NULL || out == NULL) {
        return ARGON2_MISSING_ARGS;
    }
    /* argon2id_hash_raw checks the lengths and costs against Argon2's limits and wipes its
       working memory before it returns. */
    return argon2id_hash_raw(t_cost, m_kib, lanes, password, password_len, salt, salt_len,
                             out, out_len);
}
