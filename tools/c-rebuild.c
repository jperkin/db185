/*
 * c-rebuild: pure-libnbcompat reimplementation of `pkg_admin
 * rebuild` that writes its output `pkgdb.byfile.db` to a
 * caller-supplied path rather than the privileged
 * `$PKG_DBDIR/pkgdb.byfile.db`.  Lets the writer be benchmarked
 * head-to-head against the Rust implementation without needing
 * root.
 *
 * Algorithm mirrors `pkg_admin`'s `add_pkg` /
 * `iterate_pkg_db` from `pkgtools/pkg_install/files/admin/main.c`
 * and `lib/iterate.c`:
 *
 *   1. opendir(pkgdb) + readdir, skipping the special names
 *      (pkgdb.byfile.db, .cookie, pkg-vulnerabilities) and
 *      non-directories.
 *   2. For each package dir, open +CONTENTS line by line.
 *   3. @cwd <path> sets the current directory; @cwd .  expands
 *      to <pkgdb>/<pkgname>.
 *   4. @ignore skips the next plist line.
 *   5. A non-`@` line is a file: build "cwd/name" + NUL, check
 *      isfile() || islinktodir(), and pkgdb_store(key, pkgname).
 *   6. @pkgdir <name> implements `add_pkgdir`: retrieve, and if
 *      the key was already stored as "@pkgdir ...", remove it
 *      and store back with this pkgname appended.
 *
 * Usage: c-rebuild <pkgdb-dir> <out.db>
 *
 * Like `pkg_admin rebuild`, the implementation skips
 * `@comment`/`@option`/etc. directives entirely.
 */

#include <nbcompat.h>
#include <nbcompat/db.h>

#include <dirent.h>
#include <err.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define MAX_LINE   4096
#define MAX_PATH   4096
#define MAX_VALUE  65536

static DB *db;
static const char *dbdir;

/* Equivalent of pkg_install's isfile(p) || islinktodir(p). */
static int
should_store(const char *path)
{
	struct stat sb;

	if (stat(path, &sb) == 0 && S_ISREG(sb.st_mode))
		return 1;
	if (lstat(path, &sb) == 0 && S_ISLNK(sb.st_mode)
	    && stat(path, &sb) == 0 && S_ISDIR(sb.st_mode))
		return 1;
	return 0;
}

static void
pkgdb_store(const char *key, size_t klen, const char *val, size_t vlen)
{
	DBT k = { (void *)key, klen };
	DBT v = { (void *)val, vlen };
	if (db->put(db, &k, &v, R_NOOVERWRITE) == RET_ERROR)
		err(2, "pkgdb_store(%s)", key);
}

static int
pkgdb_retrieve(const char *key, size_t klen, char *out, size_t *outlen)
{
	DBT k = { (void *)key, klen };
	DBT v;
	int rc = db->get(db, &k, &v, 0);
	if (rc == RET_ERROR)
		err(2, "pkgdb_retrieve(%s)", key);
	if (rc == RET_SPECIAL)
		return 0;
	if (v.size > *outlen)
		errx(2, "pkgdb value too large (%zu > %zu)",
		    (size_t)v.size, *outlen);
	memcpy(out, v.data, v.size);
	*outlen = v.size;
	return 1;
}

static void
pkgdb_remove(const char *key, size_t klen)
{
	DBT k = { (void *)key, klen };
	if (db->del(db, &k, 0) == RET_ERROR)
		err(2, "pkgdb_remove");
}

/*
 * Strip the trailing newline that fgets() leaves on the line,
 * returning its new length.
 */
static size_t
chomp(char *s)
{
	size_t n = strlen(s);
	while (n > 0 && (s[n - 1] == '\n' || s[n - 1] == '\r'))
		s[--n] = '\0';
	return n;
}

static void
process_pkg(const char *pkgname)
{
	char contents_path[MAX_PATH];
	char line[MAX_LINE];
	char cwd[MAX_PATH];
	char key[MAX_PATH];
	char file[MAX_PATH];
	char value[MAX_VALUE];
	size_t pkgname_len = strlen(pkgname);
	int have_cwd = 0;
	int skip_next = 0;
	FILE *f;

	snprintf(contents_path, sizeof(contents_path), "%s/%s/+CONTENTS",
	    dbdir, pkgname);
	if ((f = fopen(contents_path, "r")) == NULL)
		err(2, "fopen %s", contents_path);

	while (fgets(line, sizeof(line), f) != NULL) {
		size_t n = chomp(line);
		if (n == 0)
			continue;
		if (skip_next) {
			skip_next = 0;
			continue;
		}

		if (line[0] != '@') {
			/* File entry. */
			if (!have_cwd)
				errx(2, "%s: @cwd not yet found",
				    contents_path);
			int klen = snprintf(key, sizeof(key), "%s/%s",
			    cwd, line);
			if (klen <= 0 || (size_t)klen >= sizeof(key))
				errx(2, "%s: key too long", contents_path);
			memcpy(file, key, klen);
			file[klen] = '\0';
			if (!should_store(file))
				continue;
			/* key is "<path>\0", value is "<pkgname>\0";
			 * pkg_install stores including the NUL. */
			pkgdb_store(key, klen + 1, pkgname, pkgname_len + 1);
			continue;
		}

		/* '@' directive. */
		if (strncmp(line, "@cwd ", 5) == 0) {
			const char *arg = line + 5;
			if (strcmp(arg, ".") == 0) {
				snprintf(cwd, sizeof(cwd), "%s/%s", dbdir,
				    pkgname);
			} else {
				strncpy(cwd, arg, sizeof(cwd) - 1);
				cwd[sizeof(cwd) - 1] = '\0';
			}
			have_cwd = 1;
		} else if (strcmp(line, "@ignore") == 0) {
			skip_next = 1;
		} else if (strncmp(line, "@pkgdir ", 8) == 0) {
			const char *name = line + 8;
			if (!have_cwd)
				errx(2, "%s: @cwd not yet found", contents_path);
			int klen = snprintf(key, sizeof(key), "%s/%s",
			    cwd, name);
			if (klen <= 0 || (size_t)klen >= sizeof(key))
				errx(2, "%s: key too long", contents_path);
			size_t outlen = sizeof(value);
			if (pkgdb_retrieve(key, klen + 1, value, &outlen)) {
				if (outlen < 9 || strncmp(value, "@pkgdir ", 8) != 0)
					errx(2, "pkgdb collision on %s", key);
				/* Build "<old-without-NUL> <pkgname>\0". */
				char newval[MAX_VALUE];
				size_t old_no_nul = outlen - 1;
				if (old_no_nul + 1 + pkgname_len + 1 > sizeof(newval))
					errx(2, "@pkgdir value too long");
				memcpy(newval, value, old_no_nul);
				newval[old_no_nul] = ' ';
				memcpy(newval + old_no_nul + 1, pkgname,
				    pkgname_len);
				newval[old_no_nul + 1 + pkgname_len] = '\0';
				pkgdb_remove(key, klen + 1);
				pkgdb_store(key, klen + 1, newval,
				    old_no_nul + 1 + pkgname_len + 1);
			} else {
				/* "@pkgdir <pkgname>\0". */
				char newval[MAX_VALUE];
				int vlen = snprintf(newval, sizeof(newval),
				    "@pkgdir %s", pkgname);
				if (vlen <= 0 || (size_t)vlen >= sizeof(newval))
					errx(2, "@pkgdir value too long");
				pkgdb_store(key, klen + 1, newval, vlen + 1);
			}
		}
		/* All other @-directives are ignored, matching add_pkg. */
	}
	fclose(f);
}

static int
is_special_name(const char *name)
{
	return strcmp(name, "pkgdb.byfile.db") == 0
	    || strcmp(name, ".cookie") == 0
	    || strcmp(name, "pkg-vulnerabilities") == 0;
}

int
main(int argc, char **argv)
{
	BTREEINFO info;
	DIR *dirp;
	struct dirent *dp;

	if (argc != 3) {
		fprintf(stderr, "usage: %s <pkgdb-dir> <out.db>\n", argv[0]);
		return 1;
	}
	dbdir = argv[1];
	(void)unlink(argv[2]);

	/* Same dbopen knobs as pkg_install's pkgdb_open. */
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

	if ((dirp = opendir(dbdir)) == NULL)
		err(2, "opendir %s", dbdir);
	while ((dp = readdir(dirp)) != NULL) {
		if (strcmp(dp->d_name, ".") == 0 || strcmp(dp->d_name, "..") == 0)
			continue;
		if (is_special_name(dp->d_name))
			continue;
#if defined(DT_UNKNOWN) && defined(DT_DIR)
		if (dp->d_type != DT_UNKNOWN && dp->d_type != DT_DIR)
			continue;
#endif
		process_pkg(dp->d_name);
	}
	closedir(dirp);

	if (db->close(db) != 0)
		err(2, "db->close");
	return 0;
}
