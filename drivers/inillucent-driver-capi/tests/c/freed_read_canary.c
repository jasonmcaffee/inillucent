/*
 * The C half of freed_read_canary.rs. It frees a Rust box and then asks Rust
 * to read it, so the only read of freed memory happens in Rust code. A
 * sanitized run that prints `done` has not seen that read.
 */
#include <stdint.h>
#include <stdio.h>

void *canary_new(void);
void canary_free(void *canary);
uint64_t canary_read(const void *canary);

int main(void) {
    void *canary = canary_new();
    printf("ok read before free %llx\n", (unsigned long long)canary_read(canary));
    canary_free(canary);
    fflush(stdout);
    printf("read after free %llx\n", (unsigned long long)canary_read(canary));
    printf("done\n");
    return 0;
}
