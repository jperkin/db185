/*
 * Replay an op trace through libnbcompat's dbopen(3) btree
 * implementation.  Builds a "C reference" output we can byte-compare
 * against the captured pkg_admin pkgdb.byfile.db: if they match, the
 * trace order matches what pkg_admin emits and any remaining
 * divergence vs the Rust writer is on the Rust side.
 *
 * Trace format (matches tools/c-replay reads what
 * examples/pkgdb-byfile-trace.rs writes):
 *
 *   u8  op            0 = PUT, 1 = DEL
 *   u32 le key_len    little-endian length (includes trailing NUL)
 *   u8  key_len       key bytes
 *   if op == 0:
 *     u32 le val_len  little-endian length (includes trailing NUL)
 *     u8  val_len     value bytes
 *
 * Usage: c-replay <trace> <out.db>
 *
 * Exit status: 0 on success, 1 on usage error, 2 on I/O / dbopen
 * error.
 */

#include <nbcompat.h>
#include <nbcompat/db.h>

#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define OP_PUT 0
#define OP_DEL 1

static uint32_t
read_u32_le(const unsigned char *p)
{
	return (uint32_t)p[0] | ((uint32_t)p[1] << 8) |
	    ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

int
main(int argc, char **argv)
{
	BTREEINFO info;
	DB *db;
	DBT key, val;
	unsigned char *trace;
	size_t trace_len;
	size_t pos;
	uint64_t op_index;
	int fd;
	struct stat st;

	uint64_t stop_after = 0; /* 0 means "no limit". */
	if (argc < 3 || argc > 4) {
		fprintf(stderr,
		    "usage: %s <trace.bin> <out.db> [stop_after]\n", argv[0]);
		return 1;
	}
	if (argc == 4) {
		char *end;
		stop_after = strtoull(argv[3], &end, 10);
		if (*end != '\0') {
			fprintf(stderr, "bad stop_after %s\n", argv[3]);
			return 1;
		}
	}

	fd = open(argv[1], O_RDONLY);
	if (fd < 0)
		err(2, "open %s", argv[1]);
	if (fstat(fd, &st) < 0)
		err(2, "fstat %s", argv[1]);
	trace_len = (size_t)st.st_size;
	trace = malloc(trace_len);
	if (trace == NULL)
		err(2, "malloc %zu", trace_len);
	if (read(fd, trace, trace_len) != (ssize_t)trace_len)
		err(2, "read %s", argv[1]);
	close(fd);

	(void)unlink(argv[2]);

	/*
	 * Mirror pkg_install's pkgdb_open: btree, no dups, psize=4096,
	 * 2 MiB mpool cache, host byte order, no custom comparator.
	 */
	info.flags = 0;
	info.cachesize = 2 * 1024 * 1024;
	info.maxkeypage = 0;
	info.minkeypage = 0;
	info.psize = 4096;
	info.compare = NULL;
	info.prefix = NULL;
	info.lorder = 0;

	db = dbopen(argv[2], O_RDWR | O_CREAT, 0644, DB_BTREE, &info);
	if (db == NULL)
		err(2, "dbopen %s", argv[2]);

	pos = 0;
	op_index = 0;
	while (pos < trace_len) {
		uint8_t op;
		uint32_t klen, vlen;

		if (stop_after != 0 && op_index >= stop_after)
			break;

		op = trace[pos++];
		if (pos + 4 > trace_len)
			errx(2, "op %" PRIu64 ": truncated key length",
			    op_index);
		klen = read_u32_le(trace + pos);
		pos += 4;
		if (pos + klen > trace_len)
			errx(2, "op %" PRIu64 ": truncated key payload",
			    op_index);

		key.data = trace + pos;
		key.size = klen;
		pos += klen;

		switch (op) {
		case OP_PUT:
			if (pos + 4 > trace_len)
				errx(2,
				    "op %" PRIu64 ": truncated value length",
				    op_index);
			vlen = read_u32_le(trace + pos);
			pos += 4;
			if (pos + vlen > trace_len)
				errx(2,
				    "op %" PRIu64 ": truncated value payload",
				    op_index);
			val.data = trace + pos;
			val.size = vlen;
			pos += vlen;
			if (db->put(db, &key, &val, R_NOOVERWRITE) ==
			    RET_ERROR)
				err(2, "op %" PRIu64 ": put", op_index);
			break;
		case OP_DEL:
			if (db->del(db, &key, 0) == RET_ERROR)
				err(2, "op %" PRIu64 ": del", op_index);
			break;
		default:
			errx(2, "op %" PRIu64 ": unknown op byte 0x%02x",
			    op_index, op);
		}
		op_index++;
	}

	if (db->close(db) != 0)
		err(2, "db->close");

	free(trace);
	fprintf(stderr, "%s: %" PRIu64 " ops replayed -> %s\n", argv[0],
	    op_index, argv[2]);
	return 0;
}
