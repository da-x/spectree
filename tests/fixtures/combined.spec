Name:           combined
Version:        1.0
Release:        1%{?dist}
Summary:        An extended hello world package

License:        MIT
URL:            https://example.com
Source0:        %{name}-%{version}.tar.gz

BuildArch:      noarch
Requires:       hello

%description
An extended test package that depends on the hello package.

%prep
%setup -q

%build
# Nothing to build

%install
mkdir -p %{buildroot}/usr/share/combined
echo "Extended Hello from %{name}" > %{buildroot}/usr/share/combined/extended.txt

%files
/usr/share/combined/extended.txt

%changelog
* Mon Jan 01 2024 Test User <test@example.com> - 1.0-1
- Initial package
