/* Checks twr_argon2id against the phone-pairing test vector. Run by scripts/test-argon2.sh,
   which reads the vector from clients/shared/fixtures/phone-pairing-v2.json.

   usage: argon2_vector_test CODE SALT T_COST M_KIB LANES EXPECTED_HEX
   Prints the computed hash as hex on stdout; exits 0 only if it equals EXPECTED_HEX. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "TwrArgon2.h"

static void to_hex(const unsigned char *bytes, size_t len, char *out) {
    static const char digits[] = "0123456789abcdef";
    for (size_t i = 0; i < len; i++) {
        out[2 * i] = digits[bytes[i] >> 4];
        out[2 * i + 1] = digits[bytes[i] & 15];
    }
    out[2 * len] = '\0';
}

int main(int argc, char **argv) {
    if (argc != 7) {
        fprintf(stderr, "usage: %s CODE SALT T_COST M_KIB LANES EXPECTED_HEX\n", argv[0]);
        return 2;
    }
    const char *code = argv[1], *salt = argv[2], *expected = argv[6];
    uint32_t t_cost = (uint32_t)strtoul(argv[3], NULL, 10);
    uint32_t m_kib = (uint32_t)strtoul(argv[4], NULL, 10);
    uint32_t lanes = (uint32_t)strtoul(argv[5], NULL, 10);
    size_t len = strlen(expected) / 2;
    if (len == 0 || len > 64 || strlen(expected) != 2 * len) {
        fprintf(stderr, "EXPECTED_HEX must be 2 to 128 hex digits\n");
        return 2;
    }
    unsigned char out[64];
    char hex[129];
    int status = twr_argon2id(code, strlen(code), salt, strlen(salt), t_cost, m_kib, lanes, out, len);
    if (status != 0) {
        fprintf(stderr, "twr_argon2id failed: %d\n", status);
        return 1;
    }
    to_hex(out, len, hex);
    printf("%s\n", hex);
    int failures = 0;
    if (strcmp(hex, expected) != 0) {
        fprintf(stderr, "MISMATCH\n  expected %s\n  computed %s\n", expected, hex);
        failures++;
    }

    /* Another code gives another hash. */
    char other[64];
    snprintf(other, sizeof other, "%s", code);
    other[0] = other[0] == 'A' ? 'B' : 'A';
    unsigned char scratch[64];
    if (twr_argon2id(other, strlen(other), salt, strlen(salt), t_cost, m_kib, lanes, scratch, len) != 0 ||
        memcmp(out, scratch, len) == 0) {
        fprintf(stderr, "another code did not give another hash\n");
        failures++;
    }

    /* The wrapper refuses what Argon2 refuses instead of returning something. */
    if (twr_argon2id(code, strlen(code), salt, strlen(salt), t_cost, 1, lanes, scratch, len) == 0) {
        fprintf(stderr, "a 1 KiB memory cost was accepted\n");
        failures++;
    }
    if (twr_argon2id(code, strlen(code), "short", 5, t_cost, m_kib, lanes, scratch, len) == 0) {
        fprintf(stderr, "a 5-byte salt was accepted\n");
        failures++;
    }
    if (twr_argon2id(NULL, 0, salt, strlen(salt), t_cost, m_kib, lanes, scratch, len) == 0) {
        fprintf(stderr, "a missing password was accepted\n");
        failures++;
    }
    return failures == 0 ? 0 : 1;
}
