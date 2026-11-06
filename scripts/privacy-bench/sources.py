"""Resolve immutable, verified CLI artifacts from the approved distribution sources."""
import base64
import hashlib
import io
import json
import re
import tarfile
import urllib.error
import urllib.parse
import urllib.request
import zipfile

REGISTRY = 'https://registry.npmjs.org'
GITLAB = 'https://git.xiaoaojianghu.fun:114'
PROJECT = 87
TARGET = 'x86_64-unknown-linux-gnu'
PACKAGE = '@einsia/agent-git-linux-x64'
STABLE = re.compile(r'^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$')
MAX_ARCHIVE = 256 * 1024 * 1024


class BenchError(Exception):
    pass


def sha(data):
    return hashlib.sha256(data).hexdigest()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # A private request must never forward its authorization to an artifact host.
        raise BenchError('Distribution source redirected; credentials were not forwarded')


class Sources:
    def __init__(self, token):
        if not token or not token.strip():
            raise BenchError('BENCH_GITLAB_READ_TOKEN is required (read_api for project 87)')
        self.token = token
        self.opener = urllib.request.build_opener(NoRedirect())
        self.stable = None
        self.resolved = {}

    def get(self, url, private=False):
        expected = GITLAB if private else REGISTRY
        if (urllib.parse.urlsplit(url).scheme != 'https'
                or urllib.parse.urlsplit(url).netloc != urllib.parse.urlsplit(expected).netloc):
            raise BenchError('Distribution URL is outside the approved origin')
        headers = {'PRIVATE-TOKEN': self.token} if private else {}
        try:
            with self.opener.open(urllib.request.Request(url, headers=headers), timeout=60) as r:
                data = r.read(MAX_ARCHIVE + 1)
                if len(data) > MAX_ARCHIVE:
                    raise BenchError('Distribution response exceeds the size limit')
                return data
        except urllib.error.HTTPError as e:
            status = e.code
            e.close()
            raise BenchError(f'Distribution request failed: HTTP {status}') from None
        except (OSError, ValueError) as e:
            raise BenchError(f'Distribution transport failed ({type(e).__name__})') from None

    def api(self, path):
        return json.loads(self.get(f'{GITLAB}/api/v4/projects/{PROJECT}/{path}', True))

    def preflight(self):
        self.api('jobs?scope[]=success&per_page=1')

    def npm(self, package, version=None):
        path = urllib.parse.quote(package, safe='@')
        return json.loads(self.get(f'{REGISTRY}/{path}' + (f'/{version}' if version else '')))

    def release(self, selector):
        requested = selector.removeprefix('release:')
        if self.stable is None:
            metadata = self.npm('@einsia/agent-git')
            self.stable = sorted((v for v in metadata['versions'] if STABLE.fullmatch(v)),
                                 key=lambda v: tuple(map(int, v.split('.'))), reverse=True)
        if requested in ('latest', 'previous'):
            candidates = self.stable
            available = []
            for version in candidates:
                try:
                    package = self.npm(PACKAGE, version)
                except BenchError as e:
                    if str(e).endswith('HTTP 404'):
                        continue
                    raise
                available.append(package)
                if len(available) == (1 if requested == 'latest' else 2):
                    break
            if len(available) != (1 if requested == 'latest' else 2):
                raise BenchError(f'No available stable platform package for {selector}')
            package = available[-1]
        elif STABLE.fullmatch(requested):
            package = self.npm(PACKAGE, requested)
        else:
            raise BenchError('Release selector must name a stable semantic version, latest or previous')
        version = package['version']
        if package['name'] != PACKAGE or not STABLE.fullmatch(version):
            raise BenchError('Platform package identity is invalid')
        if STABLE.fullmatch(requested) and version != requested:
            raise BenchError('Platform package version differs from the selected version')
        dist = package['dist']
        archive = self.get(dist['tarball'])
        integrities = dist['integrity'].split()
        if not any(sri == 'sha512-' + base64.b64encode(hashlib.sha512(archive).digest()).decode()
                   for sri in integrities):
            raise BenchError('npm archive integrity verification failed')
        files = tar_members(archive, ('package/bin/agit', 'package/agit', 'package/package.json'))
        binary_names = [k for k in ('package/bin/agit', 'package/agit') if k in files]
        if len(binary_names) != 1:
            raise BenchError('Platform package has no unique CLI binary')
        info = json.loads(files['package/package.json'])
        if info['name'] != PACKAGE or info['version'] != version:
            raise BenchError('Packed package identity differs from registry metadata')
        return files[binary_names[0]], {
            'selector': selector, 'version': version, 'platform': 'linux-x64',
            'target': 'x86_64-unknown-linux-musl', 'package': PACKAGE,
            'integrity': dist['integrity'], 'source': dist['tarball'],
            'source_commit': package.get('gitHead'), 'archive_sha256': sha(archive),
        }

    def dev(self, selector):
        requested = selector.removeprefix('dev:')
        job = None
        if requested == 'latest':
            page = 1
            while job is None:
                jobs = self.api(f'jobs?scope[]=success&per_page=100&page={page}')
                if not jobs:
                    raise BenchError('No downloadable successful main dev:linux job')
                job = next((j for j in jobs if eligible(j)), None)
                page += 1
        elif requested.isdigit() and int(requested) > 0:
            job = self.api(f'jobs/{requested}')
        if job is None or not eligible(job):
            raise BenchError('Dev selector must name a successful main dev:linux job with artifacts')
        archive = self.get(f'{GITLAB}/api/v4/projects/{PROJECT}/jobs/{job["id"]}/artifacts', True)
        with zipfile.ZipFile(io.BytesIO(archive)) as z:
            entries = [x for x in z.infolist() if re.fullmatch(r'dist/agit-dev-[0-9a-f]+-x86_64-unknown-linux-gnu\.tar\.gz', x.filename)]
            if len(entries) != 1 or entries[0].file_size > MAX_ARCHIVE:
                raise BenchError('Dev artifact has no unique bounded Linux archive')
            tarball = z.read(entries[0])
        files = tar_members(tarball, ('agit', 'BUILD-INFO.txt', 'SHA256SUMS'))
        info = dict(line.split('=', 1) for line in files['BUILD-INFO.txt'].decode().splitlines())
        commit = job['commit']['id']
        if (info['channel'] != 'dev' or info['target'] != TARGET
                or not re.fullmatch(r'[0-9a-f]{40}', commit) or info['commit'] != commit
                or job['pipeline']['sha'] != commit
                or not re.fullmatch(r'\d+\.\d+\.\d+-dev\+' + commit[:12], info['version'])):
            raise BenchError('Dev build metadata differs from the selected job')
        digest = sha(files['agit'])
        if not re.fullmatch(digest + r' [ *]agit\n?', files['SHA256SUMS'].decode()):
            raise BenchError('Dev binary checksum verification failed')
        return files['agit'], {
            'selector': selector, 'version': info['version'], 'platform': 'linux-x64',
            'target': info['target'], 'source_commit': commit,
            'project_id': PROJECT, 'pipeline_id': job['pipeline']['id'], 'job_id': job['id'],
            'archive_sha256': sha(archive), 'tarball_sha256': sha(tarball),
        }

    def resolve(self, selector, directory):
        if selector in self.resolved:
            binary, record = self.resolved[selector]
        elif selector.startswith('release:'):
            binary, record = self.release(selector)
        elif selector.startswith('dev:'):
            binary, record = self.dev(selector)
        else:
            raise BenchError('Only release: and dev: selectors are accepted')
        if binary[:5] != b'\x7fELF\x02' or binary[18:20] != b'\x3e\x00':
            raise BenchError('Selected artifact is not a Linux x64 ELF binary')
        self.resolved[selector] = (binary, record)
        record = dict(record)
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / 'agit'
        path.write_bytes(binary)
        path.chmod(0o700)
        record['binary_sha256'] = sha(binary)
        return path, record


def eligible(job):
    return (job.get('name') == 'dev:linux' and job.get('ref') == 'main'
            and job.get('status') == 'success' and job.get('artifacts_file', {}).get('size', 0) > 0)


def tar_members(data, names):
    found = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as tar:
        for member in tar:
            name = member.name.removeprefix('./')
            if name not in names:
                continue
            if name in found or not member.isfile() or member.size > MAX_ARCHIVE:
                raise BenchError('Archive member is duplicated, unsafe or oversized')
            found[name] = tar.extractfile(member).read()
    return found
