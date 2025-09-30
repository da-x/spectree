Name:           hello
Version:        1.0
Release:        1%{?dist}
Summary:        A simple hello world package

License:        MIT
URL:            https://example.com
Source0:        %{name}-%{version}.tar.gz

BuildArch:      noarch

%description
A simple test package that installs a hello world text file.

%prep
%setup -q

%build
# Nothing to build

%install
mkdir -p %{buildroot}/usr/share/hello
echo "Hello World from %{name}" > %{buildroot}/usr/share/hello/hello.txt

%files
/usr/share/hello/hello.txt

%changelog
* Mon Jan 01 2024 Test User <test@example.com> - 1.0-1
- Initial package